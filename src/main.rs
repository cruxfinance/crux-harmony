use async_trait::async_trait;
use http::{HeaderValue, Uri};
use hyper::body::Incoming;
use hyper::client::conn::http1::Builder as ClientBuilder;
use hyper::server::conn::http1::Builder as ServerBuilder;
use hyper::service::service_fn;
use hyper::{body::Body, Request, Response};
use hyper_util::rt::TokioIo;
use serde::Deserialize;
use serde_yaml::Value;
use std::cmp::Ordering;
use std::collections::HashMap;
use std::fmt::Debug;
use std::fs::File;
use std::hash::Hash;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, RwLock};
use tokio::task::JoinHandle;
use tokio::time::timeout;

#[async_trait]
pub trait Node: Eq + Hash + Clone + Send + Sync + 'static {
    async fn check_health<T: Node>(
        &self,
        scores: HashMap<T, HealthScore>,
    ) -> Result<HealthScore, anyhow::Error>;
    fn is_fall_back(&self) -> bool;
    fn get_uri(&self) -> Uri;
}

#[derive(Hash, PartialEq, Eq, Clone, Debug)]
pub struct IndexedErgoNode {
    pub name: String,
    pub url: String,
    pub fall_back: bool,
}

#[derive(Deserialize)]
pub struct IndexedErgoNodeResponse {
    #[serde(rename = "indexedHeight")]
    pub indexed_height: i64,
    #[serde(rename = "fullHeight")]
    pub full_height: i64,
}

#[async_trait]
impl Node for IndexedErgoNode {
    async fn check_health<T: Node>(
        &self,
        scores: HashMap<T, HealthScore>,
    ) -> Result<HealthScore, anyhow::Error> {
        let max_height = scores
            .values()
            .map(|hs| {
                let response: IndexedErgoNodeResponse = serde_json::from_str(
                    hs.response.text.as_str(),
                )
                .unwrap_or(IndexedErgoNodeResponse {
                    indexed_height: 0_i64,
                    full_height: 0_i64,
                });
                response.indexed_height
            })
            .max()
            .unwrap_or(0_i64);
        let begin = Instant::now();
        let res = reqwest::get(format!("{}/blockchain/indexedHeight", self.url)).await?;
        let text = res.text().await?;
        let response: IndexedErgoNodeResponse =
            serde_json::from_str(text.as_str()).unwrap_or(IndexedErgoNodeResponse {
                indexed_height: 0_i64,
                full_height: 0_i64,
            });
        let latency = begin.elapsed().as_millis();
        Ok(HealthScore {
            category: if response.indexed_height >= max_height {
                HealthScoreCategory::Healthy
            } else {
                HealthScoreCategory::Unhealthy
            },
            score: latency as f64,
            response: FinishedResponse { text: text },
        })
    }

    fn is_fall_back(&self) -> bool {
        self.fall_back
    }

    fn get_uri(&self) -> Uri {
        self.url.parse::<Uri>().unwrap()
    }
}

impl IndexedErgoNode {
    fn from_value(value: &Value) -> IndexedErgoNode {
        IndexedErgoNode {
            name: value
                .get("name")
                .expect("name missing from server definition")
                .as_str()
                .expect("name should be a string")
                .to_string(),
            url: value
                .get("url")
                .expect("url missing from server definition")
                .as_str()
                .expect("url should be a string")
                .to_string(),
            fall_back: value
                .get("fall_back")
                .expect("fall_back missing from server definition")
                .as_bool()
                .expect("fall_back should be a bool"),
        }
    }
}

#[derive(Clone, PartialEq, PartialOrd, Debug)]
pub enum HealthScoreCategory {
    Healthy = 1,
    Unhealthy = 2,
    Dead = 3,
}

#[derive(Clone, Debug)]
pub struct FinishedResponse {
    pub text: String,
}

#[derive(Clone, Debug)]
pub struct HealthScore {
    pub category: HealthScoreCategory,
    pub score: f64,
    pub response: FinishedResponse,
}

impl PartialEq for HealthScore {
    fn eq(&self, other: &Self) -> bool {
        self.category == other.category && self.score == other.score
    }
}

impl PartialOrd for HealthScore {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        match self.category.partial_cmp(&other.category) {
            Some(order) => match order {
                Ordering::Equal => self.score.partial_cmp(&other.score),
                _ => Some(order),
            },
            None => None,
        }
    }
}

pub struct HealthScoreMap<T: Node> {
    best_uri: Uri,
    scores: HashMap<T, HealthScore>,
    poll_interval: i64,
}

impl<T: Node + Debug> HealthScoreMap<T> {
    pub fn new(poll_interval: i64) -> HealthScoreMap<T> {
        HealthScoreMap {
            best_uri: Uri::default(),
            scores: HashMap::<T, HealthScore>::new(),
            poll_interval: poll_interval,
        }
    }
    pub async fn add(&mut self, node: T) {
        self.scores.insert(
            node.clone(),
            match node.check_health(self.scores.clone()).await {
                Ok(value) => value,
                Err(e) => HealthScore {
                    category: HealthScoreCategory::Dead,
                    score: 0.0,
                    response: FinishedResponse {
                        text: e.to_string(),
                    },
                },
            },
        );
    }

    pub fn get_best_uri(&self) -> Uri {
        self.best_uri.clone()
    }

    pub async fn update_best_uri(&mut self) -> () {
        let mut sorted_scores = Vec::from_iter(self.scores.iter());
        sorted_scores.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap());

        self.best_uri = sorted_scores
            .iter()
            .filter(|s| s.1.category == HealthScoreCategory::Healthy && !s.0.is_fall_back())
            .collect::<Vec<&(&T, &HealthScore)>>()
            .first()
            .unwrap_or(&sorted_scores.first().unwrap())
            .0
            .get_uri();
    }

    pub async fn update_health_scores(&mut self) -> () {
        let (tx, mut rx) = mpsc::unbounded_channel();
        {
            let handles: Vec<JoinHandle<_>> = self
                .scores
                .keys()
                .map(|s| {
                    let tx_c = tx.clone();
                    let scores_c = self.scores.clone();
                    let s_c = s.clone();
                    tokio::spawn(async move {
                        let _ = tx_c.send((s_c.clone(), s_c.check_health(scores_c.clone()).await));
                    })
                })
                .collect();
            let mut new_scores = HashMap::<T, HealthScore>::new();

            let _ = timeout(Duration::from_secs(self.poll_interval as u64), async {
                while let Some(value) = rx.recv().await {
                    new_scores.insert(
                        value.0.clone(),
                        match value.1 {
                            Ok(value) => value,
                            Err(e) => HealthScore {
                                category: HealthScoreCategory::Dead,
                                score: 0.0,
                                response: FinishedResponse {
                                    text: e.to_string(),
                                },
                            },
                        },
                    );
                    self.scores = new_scores.clone();
                    let _ = self.update_best_uri().await;
                }
            })
            .await;

            self.scores.keys().for_each(|k| {
                if !new_scores.contains_key(k) {
                    new_scores.insert(
                        k.clone(),
                        HealthScore {
                            category: HealthScoreCategory::Dead,
                            score: 0.0,
                            response: FinishedResponse {
                                text: "Timed out".to_string(),
                            },
                        },
                    );
                }
            });

            handles.iter().for_each(|jh| {
                jh.abort();
            });

            self.scores = new_scores;
            let _ = self.update_best_uri().await;
        }
    }
}

async fn proxy<T: Body + std::marker::Send>(
    client: ClientBuilder,
    req: Request<T>,
    url: Arc<RwLock<Uri>>,
) -> Result<Response<Incoming>, Box<dyn std::error::Error + Send + Sync>>
where
    <T as Body>::Data: Send,
    <T as Body>::Error: std::error::Error + Send + Sync,
{
    let headers = req.headers().clone();
    println!("headers: {:?}", headers);

    let url = { url.read().await };

    let host = url.host().expect("uri has no host");
    let port = url.port_u16().unwrap_or(80);

    let address = format!("{}:{}", host, port);

    // Open a TCP connection to the remote host
    let stream = TcpStream::connect(address).await?;

    // Use an adapter to access something implementing `tokio::io` traits as if they implement
    // `hyper::rt` IO traits.
    let io = TokioIo::new(stream);

    // Perform a TCP handshake
    let (mut sender, conn) = client.handshake(io).await?;

    // Spawn a task to poll the connection, driving the HTTP state
    tokio::task::spawn(async move {
        if let Err(err) = conn.await {
            println!("Connection failed: {:?}", err);
        }
    });

    let authority = url.authority().unwrap().clone();

    let target_url = Uri::builder()
        .scheme(url.scheme().unwrap().as_str())
        .authority(authority.clone())
        .path_and_query(req.uri().path_and_query().unwrap().as_str())
        .build()?;

    let mut headers = req.headers().clone();
    headers.insert(
        hyper::header::HOST,
        HeaderValue::from_str(authority.as_str())?,
    );
    let mut request_builder = Request::builder()
        .method(req.method())
        .uri(target_url)
        .body(req.into_body())
        .unwrap();

    *request_builder.headers_mut() = headers;

    let response = sender.send_request(request_builder).await?;

    Ok(response)
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let addr = SocketAddr::from(([127, 0, 0, 1], 8100));

    let listener = TcpListener::bind(addr).await?;

    let config_file = File::open("config.yaml")?;
    let config: Value = serde_yaml::from_reader(config_file)?;
    let poll_interval = config
        .get("poll_interval")
        .expect("Config file missing field poll_interval")
        .as_i64()
        .expect("poll_interval is not a valid i64");

    let mut node_map = HealthScoreMap::new(poll_interval);

    let indexed_ergo = config
        .get("indexed_ergo")
        .expect("missing indexed_ergo in config");
    let servers = indexed_ergo
        .get("servers")
        .expect("missing servers in config")
        .as_sequence()
        .expect("servers should be a list");

    for s in servers.iter() {
        println!("Adding server: {:?}", s);
        node_map.add(IndexedErgoNode::from_value(s)).await;
    }

    node_map.update_best_uri().await;

    let best_node_uri = Arc::new(RwLock::new(node_map.get_best_uri()));

    println!("Listening on http://{}", addr);

    let node_uri_setter = best_node_uri.clone();

    tokio::task::spawn(async move {
        loop {
            node_map.update_health_scores().await;
            {
                let mut new_uri = node_uri_setter.write().await;
                *new_uri = node_map.get_best_uri();
                println!("new_uri = {}", new_uri);
            }
        }
    });

    loop {
        let (stream, _) = listener.accept().await?;

        let io = TokioIo::new(stream);

        let url = best_node_uri.clone();

        println!("url = {:?}", url);

        tokio::task::spawn(async move {
            // Finally, we bind the incoming connection to our `hello` service
            if let Err(err) = ServerBuilder::new()
                // `service_fn` converts our function in a `Service`
                .serve_connection(
                    io,
                    service_fn(move |req| {
                        let mut builder = ClientBuilder::new();
                        let url = { url.clone() };
                        let client = builder.title_case_headers(true).preserve_header_case(true);
                        proxy(client.clone(), req, url)
                    }),
                )
                .await
            {
                println!("Error serving connection: {:?}", err);
            }
        });
    }
}
