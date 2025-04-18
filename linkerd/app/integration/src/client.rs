use super::*;
use http::{Request, Response};
use linkerd_app_core::{proxy::http::TokioExecutor, svc::http::BoxBody};
use parking_lot::Mutex;
use std::io;
use tokio::{net::TcpStream, task::JoinHandle};
use tokio_rustls::rustls::{self, ClientConfig};
use tracing::info_span;

type ClientError = hyper_util::client::legacy::Error;
type Sender = mpsc::UnboundedSender<(
    Request<BoxBody>,
    oneshot::Sender<Result<Response<hyper::body::Incoming>, ClientError>>,
)>;

#[derive(Clone)]
pub struct TlsConfig {
    client_config: Arc<ClientConfig>,
    name: rustls::pki_types::ServerName<'static>,
}

impl TlsConfig {
    pub fn new(client_config: Arc<ClientConfig>, name: &'static str) -> Self {
        let name =
            rustls::pki_types::ServerName::try_from(name).expect("name must be a valid DNS name");
        TlsConfig {
            client_config,
            name,
        }
    }
}

pub fn new<T: Into<String>>(addr: SocketAddr, auth: T) -> Client {
    http2(addr, auth.into())
}

pub fn http1<T: Into<String>>(addr: SocketAddr, auth: T) -> Client {
    Client::new(
        addr,
        auth.into(),
        Run::Http1 {
            absolute_uris: false,
        },
        None,
    )
}

pub fn http1_tls<T: Into<String>>(addr: SocketAddr, auth: T, tls: TlsConfig) -> Client {
    Client::new(
        addr,
        auth.into(),
        Run::Http1 {
            absolute_uris: false,
        },
        Some(tls),
    )
}

/// This sends `GET http://foo.com/ HTTP/1.1` instead of just `GET / HTTP/1.1`.
pub fn http1_absolute_uris<T: Into<String>>(addr: SocketAddr, auth: T) -> Client {
    Client::new(
        addr,
        auth.into(),
        Run::Http1 {
            absolute_uris: true,
        },
        None,
    )
}

pub fn http2<T: Into<String>>(addr: SocketAddr, auth: T) -> Client {
    Client::new(addr, auth.into(), Run::Http2, None)
}

pub fn http2_tls<T: Into<String>>(addr: SocketAddr, auth: T, tls: TlsConfig) -> Client {
    Client::new(addr, auth.into(), Run::Http2, Some(tls))
}

pub struct Client {
    addr: SocketAddr,
    run: Run,
    authority: String,
    /// This is a future that completes when the associated connection for
    /// this Client has been dropped.
    running: Running,
    tx: Sender,
    task: JoinHandle<()>,
    version: http::Version,
    tls: Option<TlsConfig>,
}

pub struct Reconnect {
    addr: SocketAddr,
    authority: String,
    run: Run,
    tls: Option<TlsConfig>,
}

impl Client {
    fn new(addr: SocketAddr, authority: String, r: Run, tls: Option<TlsConfig>) -> Client {
        let v = match r {
            Run::Http1 { .. } => http::Version::HTTP_11,
            Run::Http2 => http::Version::HTTP_2,
        };
        let (tx, task, running) = run(addr, r, tls.clone());
        Client {
            addr,
            run: r,
            authority,
            running,
            task,
            tx,
            version: v,
            tls,
        }
    }

    pub async fn get(&self, path: &str) -> String {
        let req = self.request_builder(path);
        let res = self.request(req.method("GET")).await.expect("response");
        assert!(
            res.status().is_success(),
            "client.get({:?}) expects 2xx, got \"{}\"",
            path,
            res.status(),
        );
        let stream = res.into_parts().1;
        http_util::body_to_string(stream).await.unwrap()
    }

    pub fn request(
        &self,
        builder: http::request::Builder,
    ) -> impl Future<Output = Result<Response<hyper::body::Incoming>, ClientError>> + Send + 'static
    {
        let req = builder.body(BoxBody::empty()).unwrap();
        self.send_req(req)
    }

    pub async fn request_body<B>(&self, req: Request<B>) -> Response<hyper::body::Incoming>
    where
        B: Body + Send + 'static,
        B::Data: Send + 'static,
        B::Error: Into<Error>,
    {
        let req = req.map(BoxBody::new);
        self.send_req(req).await.expect("response")
    }

    pub fn request_builder(&self, path: &str) -> http::request::Builder {
        let b = ::http::Request::builder();

        if self.tls.is_some() {
            b.uri(format!("https://{}{}", self.authority, path).as_str())
                .version(self.version)
        } else {
            b.uri(format!("http://{}{}", self.authority, path).as_str())
                .version(self.version)
        }
    }

    #[tracing::instrument(skip(self, req))]
    pub(crate) fn send_req<B>(
        &self,
        mut req: Request<B>,
    ) -> impl Future<Output = Result<Response<hyper::body::Incoming>, ClientError>> + Send + 'static
    where
        B: Body + Send + 'static,
        B::Data: Send + 'static,
        B::Error: Into<Error>,
    {
        if req.uri().scheme().is_none() {
            if self.tls.is_some() {
                *req.uri_mut() = format!("https://{}{}", self.authority, req.uri().path())
                    .parse()
                    .unwrap();
            } else {
                *req.uri_mut() = format!("http://{}{}", self.authority, req.uri().path())
                    .parse()
                    .unwrap();
            };
        }
        tracing::debug!(headers = ?req.headers(), "request");
        let (tx, rx) = oneshot::channel();
        let req = req.map(BoxBody::new);
        let _ = self.tx.send((req, tx));
        async { rx.await.expect("request cancelled") }.in_current_span()
    }

    pub async fn wait_for_closed(self) {
        self.running.await
    }

    /// Shut down the client, returning a type that can be used to initiate a
    /// new client connection to that target.
    pub async fn shutdown(self) -> Reconnect {
        let Self {
            tx,
            task,
            running,
            run,
            addr,
            authority,
            tls,
            ..
        } = self;
        // signal the client task to shut down now.
        drop(tx);
        task.await.unwrap();
        running.await;
        Reconnect {
            authority,
            run,
            addr,
            tls,
        }
    }

    pub fn target_addr(&self) -> SocketAddr {
        self.addr
    }
}

impl Reconnect {
    pub fn reconnect(self) -> Client {
        Client::new(self.addr, self.authority, self.run, self.tls)
    }
}

#[derive(Debug, Clone, Copy)]
enum Run {
    Http1 { absolute_uris: bool },
    Http2,
}

pub type Running = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

fn run(
    addr: SocketAddr,
    version: Run,
    tls: Option<TlsConfig>,
) -> (Sender, JoinHandle<()>, Running) {
    let (tx, rx) = mpsc::unbounded_channel::<(
        Request<BoxBody>,
        oneshot::Sender<Result<Response<hyper::body::Incoming>, ClientError>>,
    )>();

    let test_name = thread_name();
    let absolute_uris = if let Run::Http1 { absolute_uris } = version {
        absolute_uris
    } else {
        false
    };

    let (running_tx, running) = {
        let (tx, rx) = oneshot::channel();
        let rx = Box::pin(rx.map(|_| ()));
        (tx, rx)
    };

    let conn = Conn {
        addr,
        absolute_uris,
        running: Arc::new(Mutex::new(Some(running_tx))),
        tls,
    };

    let http2_only = match version {
        Run::Http1 { .. } => false,
        Run::Http2 => true,
    };

    let span = info_span!("test client", peer_addr = %addr, ?version, test = %test_name);
    let work = async move {
        let client = hyper_util::client::legacy::Client::builder(TokioExecutor::new())
            .http2_only(http2_only)
            .build::<Conn, BoxBody>(conn);
        tracing::trace!("client task started");
        let mut rx = rx;
        let (drain_tx, drain) = drain::channel();
        // Scope so that the original `Watch` side of the `drain` channel which
        // is cloned into spawned tasks is dropped when the client loop ends.
        // Otherwise, the `drain().await` would never finish, since one `Watch`
        // instance would remain un-dropped.
        async move {
            while let Some((req, cb)) = rx.recv().await {
                tracing::trace!(?req);
                let req = client.request(req);
                tokio::spawn(
                    cancelable(drain.clone(), async move {
                        let result = req.await;
                        let _ = cb.send(result);
                        Ok::<(), ()>(())
                    })
                    .in_current_span(),
                );
            }
        }
        .await;

        tracing::trace!("client task shutting down");
        drain_tx.drain().await;
        tracing::trace!("client shutdown completed");
    };
    let task = tokio::spawn(work.instrument(span));
    (tx, task, running)
}

#[derive(Clone)]
struct Conn {
    addr: SocketAddr,
    absolute_uris: bool,
    tls: Option<TlsConfig>,
    running: Arc<Mutex<Option<oneshot::Sender<()>>>>,
}

impl tower::Service<hyper::Uri> for Conn {
    type Response = hyper_util::rt::TokioIo<RunningIo>;
    type Error = io::Error;
    type Future = Pin<
        Box<dyn Future<Output = io::Result<hyper_util::rt::TokioIo<RunningIo>>> + Send + 'static>,
    >;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, _: hyper::Uri) -> Self::Future {
        let tls = self.tls.clone();
        let conn = TcpStream::connect(self.addr);
        let abs_form = self.absolute_uris;
        let running = self
            .running
            .lock()
            .take()
            .expect("test client cannot connect twice");
        Box::pin(async move {
            let io = conn.await?;

            let io = if let Some(TlsConfig {
                name,
                client_config,
            }) = tls
            {
                let io = tokio_rustls::TlsConnector::from(client_config.clone())
                    .connect(name, io)
                    .await?;
                Box::pin(io) as Pin<Box<dyn Io + Send + 'static>>
            } else {
                Box::pin(io) as Pin<Box<dyn Io + Send + 'static>>
            };
            Ok(hyper_util::rt::TokioIo::new(RunningIo {
                io,
                abs_form,
                _running: Some(running),
            }))
        })
    }
}

impl hyper_util::client::legacy::connect::Connection for RunningIo {
    fn connected(&self) -> hyper_util::client::legacy::connect::Connected {
        // Setting `proxy` to true will configure Hyper to use absolute-form
        // URIs on this connection.
        hyper_util::client::legacy::connect::Connected::new().proxy(self.abs_form)
    }
}
