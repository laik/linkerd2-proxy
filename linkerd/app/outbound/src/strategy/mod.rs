use crate::endpoint::TcpEndpoint;
use futures::prelude::*;
use linkerd2_app_core::{
    buffer,
    config::ProxyConfig,
    drain,
    proxy::{http, identity},
    svc,
    transport::{tls, BoxedIo},
    Error, Never, ProxyMetrics,
};
use linkerd2_duplex::Duplex;
use linkerd2_strategy::{Detect, Endpoint, Strategy, Target};
use rand::{distributions::Distribution, rngs::SmallRng};
use std::{
    net::SocketAddr,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};
use tokio::{
    io,
    net::TcpStream,
    sync::{watch, Mutex},
};
use tracing::{debug, info, warn};

#[derive(Clone)]
pub struct Router<S> {
    get_strategy: S,
    inner: Inner,
}

#[derive(Clone)]
pub struct Accept {
    strategy: watch::Receiver<Strategy>,
    inner: Inner,
    http: Arc<
        Mutex<
            Option<buffer::Buffer<http::Request<http::Body>, http::Response<http::boxed::Payload>>>,
        >,
    >,
}

#[derive(Clone)]
struct Inner {
    config: ProxyConfig,
    identity: tls::Conditional<identity::Local>,
    metrics: ProxyMetrics,
    rng: SmallRng,
    drain: drain::Watch,
}

#[allow(dead_code)]
#[derive(Copy, Clone, Debug)]
enum Protocol {
    Unknown,
    Http(http::Version),
}

// The router is shared and its responses are buffered/cached so that multiple
// connections use the same accept object.
impl<S> tower::Service<SocketAddr> for Router<S>
where
    S: tower::Service<SocketAddr, Response = watch::Receiver<Strategy>>,
    S::Error: Into<Error>,
    S::Future: Send + 'static,
{
    type Response = Accept;
    type Error = S::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Accept, S::Error>> + Send + 'static>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), S::Error>> {
        self.get_strategy.poll_ready(cx)
    }

    fn call(&mut self, target: SocketAddr) -> Self::Future {
        // TODO dispatch timeout
        let strategy = self.get_strategy.call(target);

        let inner = self.inner.clone();
        Box::pin(async move {
            let strategy = strategy.await?;

            return Ok(Accept {
                inner,
                strategy,
                http: Arc::new(Mutex::new(None)),
            });
        })
    }
}

impl tower::Service<TcpStream> for Accept {
    type Response = ();
    type Error = Never;
    type Future = Pin<Box<dyn Future<Output = Result<(), Never>> + Send + 'static>>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Never>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, tcp: TcpStream) -> Self::Future {
        let accept = self.clone();

        Box::pin(async move {
            // TODO metrics...

            let (protocol, io) = match accept.detect(tcp).await {
                Err(error) => {
                    info!(%error, "Protocol detection error");
                    return Ok(());
                }
                Ok((protocol, io)) => (protocol, io),
            };

            let res = match (protocol, io) {
                (Protocol::Unknown, io) => accept.proxy_tcp(io).await,
                (Protocol::Http(version), io) => match version {
                    http::Version::Http1 => accept.proxy_http1(io).await,
                    http::Version::H2 => accept.proxy_h2(io).await,
                },
            };

            match res {
                Err(error) => info!(%error, "Connection closed"),
                Ok(()) => debug!("Connection closed"),
            }

            Ok(())
        })
    }
}

impl Accept {
    async fn detect(&self, tcp: TcpStream) -> Result<(Protocol, BoxedIo), Error> {
        use linkerd2_app_core::transport::io::Peekable;

        if let Detect::Opaque = self.strategy.borrow().detect {
            return Ok((Protocol::Unknown, BoxedIo::new(tcp)));
        }

        // TODO sniff  SNI.

        // TODO take advantage TcpStream::peek to avoid allocating a buf per
        // peek.
        //
        // A large buffer is needed, to fit the first line of an arbitrary HTTP
        // message (i.e. with a long URI).
        let peek = tcp.peek(8192);
        let io = tokio::time::timeout(self.inner.config.detect_protocol_timeout, peek).await??;

        let proto = http::Version::from_prefix(io.prefix().as_ref())
            .map(Protocol::Http)
            .unwrap_or(Protocol::Unknown);

        Ok((proto, BoxedIo::new(io)))
    }

    async fn proxy_tcp(mut self, io: BoxedIo) -> Result<(), Error> {
        // There's no need to watch for updates with TCP streams, since the routing decision is made
        // instantaneously.
        let Strategy {
            addr, mut target, ..
        } = self.strategy.borrow().clone();

        loop {
            target = match target {
                Target::LogicalSplit(split) => {
                    let idx = split.weights.sample(&mut self.inner.rng);
                    debug_assert!(idx < split.targets.len());
                    split.targets[idx].clone()
                }

                Target::Concrete(concrete) => {
                    // TODO TCP discovery/balancing.
                    warn!(%concrete.authority, "TCP load balancing not supported yet; forwarding");
                    Target::Endpoint(Arc::new(Endpoint {
                        addr,
                        identity: None,
                        metric_labels: Default::default(),
                    }))
                }

                Target::Endpoint(endpoint) => {
                    let id = endpoint
                        .identity
                        .clone()
                        .map(tls::Conditional::Some)
                        .unwrap_or_else(|| {
                            tls::Conditional::None(
                                tls::ReasonForNoPeerName::NotProvidedByServiceDiscovery.into(),
                            )
                        });

                    let dst_io = self.connect(addr, id).await?;
                    Duplex::new(io, dst_io).await?;

                    return Ok(());
                }
            }
        }
    }

    async fn connect(
        self,
        addr: SocketAddr,
        peer_identity: tls::PeerIdentity,
    ) -> Result<impl io::AsyncRead + io::AsyncWrite + Send, Error> {
        use tower::{util::ServiceExt, Service};

        let Inner {
            config,
            identity,
            metrics,
            ..
        } = self.inner;

        let mut connect = svc::connect(config.connect.keepalive)
            // Initiates mTLS if the target is configured with identity.
            .push(tls::client::ConnectLayer::new(identity))
            // Limits the time we wait for a connection to be established.
            .push_timeout(config.connect.timeout)
            .push(metrics.transport.layer_connect(crate::TransportLabels))
            .into_inner();

        let endpoint = TcpEndpoint {
            addr,
            identity: peer_identity,
        };
        let io = connect.ready_and().await?.call(endpoint).await?;

        Ok(io)
    }

    async fn proxy_http1(self, io: BoxedIo) -> Result<(), Error> {
        // TODO
        // - create an HTTP server
        // - dispatches to a service that holds the strategy watch...
        // - buffered/cached...
        let http_service = self.http_service().await;

        let mut conn = hyper::server::conn::Http::new()
            .with_executor(http::trace::Executor::new())
            .http1_only(true)
            .serve_connection(
                io,
                http::glue::HyperServerSvc::new(http::upgrade::Service::new(
                    http_service,
                    self.inner.drain.clone(),
                )),
            )
            .with_upgrades();

        tokio::select! {
            res = &mut conn => { res.map_err(Into::into) }
            handle = self.inner.drain.signal() => {
                Pin::new(&mut conn).graceful_shutdown();
                handle.release_after(conn).await.map_err(Into::into)
            }
        }
    }

    async fn proxy_h2(self, io: BoxedIo) -> Result<(), Error> {
        // TODO
        // - create an HTTP server
        // - dispatches to a service that holds the strategy watch...
        // - buffered/cached...
        let http_service = self.http_service().await;

        let mut conn = hyper::server::conn::Http::new()
            .with_executor(http::trace::Executor::new())
            .http2_only(true)
            .http2_initial_stream_window_size(
                self.inner
                    .config
                    .server
                    .h2_settings
                    .initial_stream_window_size,
            )
            .http2_initial_connection_window_size(
                self.inner
                    .config
                    .server
                    .h2_settings
                    .initial_connection_window_size,
            )
            .serve_connection(io, http::glue::HyperServerSvc::new(http_service));

        tokio::select! {
            res = &mut conn => { res.map_err(Into::into) }
            handle = self.inner.drain.signal() => {
                Pin::new(&mut conn).graceful_shutdown();
                handle.release_after(conn).await.map_err(Into::into)
            }
        }
    }

    async fn http_service(
        &self,
    ) -> buffer::Buffer<http::Request<http::Body>, http::Response<http::boxed::Payload>> {
        let mut cache = self.http.lock().await;

        if let Some(ref buffer) = *cache {
            return buffer.clone();
        }

        let (buffer, task) = buffer::new(
            HttpService(self.strategy.clone()),
            self.inner.config.buffer_capacity,
            None,
        );
        tokio::spawn(task);
        *cache = Some(buffer.clone());

        buffer
    }
}

#[derive(Clone, Debug)]
struct HttpService(watch::Receiver<Strategy>);

impl tower::Service<http::Request<http::Body>> for HttpService {
    type Response = http::Response<http::boxed::Payload>;
    type Error = Error;
    type Future = Pin<
        Box<
            dyn Future<Output = Result<http::Response<http::boxed::Payload>, Error>>
                + Send
                + 'static,
        >,
    >;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: http::Request<http::Body>) -> Self::Future {
        Box::pin(async move {
            let _ = req;
            unimplemented!();
        })
    }
}
