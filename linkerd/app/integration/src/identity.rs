use super::*;
use parking_lot::Mutex;
use std::{
    collections::VecDeque,
    fs, io,
    path::{Path, PathBuf},
    time::SystemTime,
};

use linkerd2_proxy_api::identity as pb;
use linkerd_meshtls_rustls::creds::default_provider_for_test;
use tokio_rustls::rustls::{self, server::WebPkiClientVerifier};
use tonic as grpc;

pub struct Identity {
    pub env: TestEnv,
    pub certify_rsp: pb::CertifyResponse,
    pub client_config: Arc<rustls::ClientConfig>,
    pub server_config: Arc<rustls::ServerConfig>,
}

#[derive(Clone, Default)]
pub struct Controller {
    expect_calls: Arc<Mutex<VecDeque<Certify>>>,
}

type Certify = Box<
    dyn FnMut(
            pb::CertifyRequest,
        ) -> Pin<
            Box<
                dyn Future<Output = Result<grpc::Response<pb::CertifyResponse>, grpc::Status>>
                    + Send,
            >,
        > + Send,
>;

struct Certificates {
    pub leaf: Vec<u8>,
    pub intermediates: Vec<Vec<u8>>,
}

impl Certificates {
    pub fn load<P>(p: P) -> Result<Certificates, io::Error>
    where
        P: AsRef<Path>,
    {
        let f = fs::File::open(p)?;
        let mut r = io::BufReader::new(f);
        let mut certs = rustls_pemfile::certs(&mut r);
        let leaf = certs
            .next()
            .expect("no leaf cert in pemfile")
            .map_err(|_| io::Error::other("rustls error reading certs"))?
            .as_ref()
            .to_vec();
        let intermediates = certs
            .map(|cert| cert.map(|cert| cert.as_ref().to_vec()))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| io::Error::other("rustls error reading certs"))?;

        Ok(Certificates {
            leaf,
            intermediates,
        })
    }

    pub fn chain(&self) -> Vec<rustls::pki_types::CertificateDer<'static>> {
        let mut chain = Vec::with_capacity(self.intermediates.len() + 1);
        chain.push(self.leaf.clone());
        chain.extend(self.intermediates.clone());
        chain
            .into_iter()
            .map(rustls::pki_types::CertificateDer::from)
            .collect()
    }

    pub fn response(&self) -> pb::CertifyResponse {
        pb::CertifyResponse {
            leaf_certificate: self.leaf.clone(),
            intermediate_certificates: self.intermediates.clone(),
            ..Default::default()
        }
    }
}

impl Identity {
    fn load_key<P>(p: P) -> rustls::pki_types::PrivateKeyDer<'static>
    where
        P: AsRef<Path>,
    {
        let p8 = fs::read(&p).expect("read key");
        rustls::pki_types::PrivateKeyDer::try_from(p8).expect("decode key")
    }

    fn configs(
        trust_anchors: &str,
        certs: &Certificates,
        key: rustls::pki_types::PrivateKeyDer<'static>,
    ) -> (Arc<rustls::ClientConfig>, Arc<rustls::ServerConfig>) {
        use std::io::Cursor;
        let mut roots = rustls::RootCertStore::empty();
        let trust_anchors = rustls_pemfile::certs(&mut Cursor::new(trust_anchors))
            .collect::<Result<Vec<_>, _>>()
            .expect("error parsing pemfile");
        let (added, skipped) = roots.add_parsable_certificates(trust_anchors);
        assert_ne!(added, 0, "trust anchors must include at least one cert");
        assert_eq!(skipped, 0, "no certs in pemfile should be invalid");

        let provider = default_provider_for_test();

        let client_config = rustls::ClientConfig::builder_with_provider(provider.clone())
            .with_safe_default_protocol_versions()
            .expect("client config must be valid")
            .with_root_certificates(roots.clone())
            .with_no_client_auth();

        let client_cert_verifier =
            WebPkiClientVerifier::builder_with_provider(Arc::new(roots), provider.clone())
                .allow_unauthenticated()
                .build()
                .expect("server verifier must be valid");

        let server_config = rustls::ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .expect("server config must be valid")
            .with_client_cert_verifier(client_cert_verifier)
            .with_single_cert(certs.chain(), key)
            .unwrap();

        (Arc::new(client_config), Arc::new(server_config))
    }

    pub fn new(dir: &'static str, local_name: String) -> Self {
        let (id_dir, token, trust_anchors, certs, key) = {
            let path_to_string = |path: &PathBuf| {
                path.as_path()
                    .to_owned()
                    .into_os_string()
                    .into_string()
                    .unwrap()
            };
            let mut id = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
            id.push("src");
            id.push("data");

            id.push("ca1.pem");
            let trust_anchors = fs::read_to_string(&id).expect("read trust anchors");

            id.set_file_name(dir);
            let id_dir = path_to_string(&id);

            id.push("token.txt");
            let token = path_to_string(&id);

            id.set_file_name("ca1-cert.pem");
            let certs = Certificates::load(&id).expect("read cert");

            id.set_file_name("key.p8");
            let key = Identity::load_key(&id);

            (id_dir, token, trust_anchors, certs, key)
        };

        let certify_rsp = certs.response();
        let (client_config, server_config) = Identity::configs(&trust_anchors, &certs, key);
        let mut env = TestEnv::default();

        env.put(app::env::ENV_IDENTITY_DIR, id_dir);
        env.put(app::env::ENV_IDENTITY_TOKEN_FILE, token);
        env.put(app::env::ENV_IDENTITY_TRUST_ANCHORS, trust_anchors);
        env.put(app::env::ENV_POLICY_WORKLOAD, format!("test:{local_name}"));
        env.put(app::env::ENV_IDENTITY_IDENTITY_LOCAL_NAME, local_name);

        Self {
            env,
            certify_rsp,
            client_config,
            server_config,
        }
    }

    pub fn service(&self) -> Controller {
        let rsp = self.certify_rsp.clone();
        Controller::default().certify(move |_req| {
            let expiry = SystemTime::now() + Duration::from_secs(666);
            pb::CertifyResponse {
                valid_until: Some(expiry.into()),
                ..rsp
            }
        })
    }
}

impl Controller {
    pub fn certify<F>(self, f: F) -> Self
    where
        F: FnOnce(pb::CertifyRequest) -> pb::CertifyResponse + Send + 'static,
    {
        self.certify_async(move |req| async { Ok::<_, grpc::Status>(f(req)) })
    }

    pub fn certify_async<F, U>(self, f: F) -> Self
    where
        F: FnOnce(pb::CertifyRequest) -> U + Send + 'static,
        U: TryFuture<Ok = pb::CertifyResponse> + Send + 'static,
        U::Error: fmt::Display + Send,
    {
        let mut f = Some(f);
        let func: Certify = Box::new(move |req| {
            // It's a shame how `FnBox` isn't actually a thing yet, otherwise this
            // closure could be one (and not a `FnMut`).
            let f = f.take().expect("called twice?");
            let fut = f(req)
                .map_ok(grpc::Response::new)
                .map_err(|e| grpc::Status::new(grpc::Code::Internal, format!("{e}")));
            Box::pin(fut)
        });
        self.expect_calls.lock().push_back(func);
        self
    }

    pub async fn run(self) -> controller::Listening {
        tracing::debug!("running support identity service");
        controller::run(
            pb::identity_server::IdentityServer::new(self),
            "support identity service",
            None,
        )
        .await
    }
}

#[tonic::async_trait]
impl pb::identity_server::Identity for Controller {
    async fn certify(
        &self,
        req: grpc::Request<pb::CertifyRequest>,
    ) -> Result<grpc::Response<pb::CertifyResponse>, grpc::Status> {
        let f = self
            .expect_calls
            .lock()
            .pop_front()
            .map(|mut f| f(req.into_inner()));
        if let Some(f) = f {
            return f.await;
        }

        Err(grpc::Status::new(
            grpc::Code::Unavailable,
            "unit test identity service has no results",
        ))
    }
}
