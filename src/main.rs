use http::{Error, HeaderValue, Uri};
use hyper::body::Incoming;
use hyper::client::conn::http1::Builder as ClientBuilder;
use hyper::server::conn::http1::Builder as ServerBuilder;
use hyper::service::service_fn;
use hyper::{body::Body, Request, Response};
use hyper_util::rt::TokioIo;
use std::convert::Infallible;
use std::net::SocketAddr;
use tokio::net::{TcpListener, TcpStream};

// async fn get_response(
//     client: ClientBuilder,
//     req: Request,
//     target_url: &str,
//     path: &str,
// ) -> Result<Response<Body>, hyper::Error> {
//     let target_url = format!("{}{}", target_url, path);
//     let headers = req.headers().clone();
//     let mut request_builder = Request::builder()
//         .method(req.method())
//         .uri(target_url)
//         .body(req.into_body())
//         .unwrap();

//     *request_builder.headers_mut() = headers;
//     let response = client.request(request_builder).await?;
//     let body = hyper::body::to_bytes(response.into_body()).await?;
//     let body = String::from_utf8(body.to_vec()).unwrap();

//     let mut resp = Response::new(Body::from(body));
//     *resp.status_mut() = http::StatusCode::OK;
//     Ok(resp)
// }

async fn proxy<T: Body + std::marker::Send>(
    client: ClientBuilder,
    req: Request<T>,
) -> Result<Response<Incoming>, Box<dyn std::error::Error + Send + Sync>>
where
    <T as Body>::Data: Send,
    <T as Body>::Error: std::error::Error + Send + Sync,
{
    let headers = req.headers().clone();
    println!("headers: {:?}", headers);

    let path = req.uri().path().to_string();
    let url = "http://192.168.1.137:9053".parse::<hyper::Uri>()?;

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
    println!("{}", target_url);

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

    println!("Listening on http://{}", addr);

    loop {
        let (stream, _) = listener.accept().await?;

        let io = TokioIo::new(stream);

        tokio::task::spawn(async move {
            // Finally, we bind the incoming connection to our `hello` service
            if let Err(err) = ServerBuilder::new()
                // `service_fn` converts our function in a `Service`
                .serve_connection(
                    io,
                    service_fn(move |req| {
                        let mut builder = ClientBuilder::new();
                        let client = builder.title_case_headers(true).preserve_header_case(true);
                        proxy(client.clone(), req)
                    }),
                )
                .await
            {
                println!("Error serving connection: {:?}", err);
            }
        });
    }
}
