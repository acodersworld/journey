use std::{
    error::Error,
    future::Future,
    future::poll_fn,
    path::Path,
    pin::Pin,
    net::SocketAddr,
};

use bytes::Bytes;
use futures_util::{stream, Stream};
use h2::client::{self, SendRequest};
use http::{header, Method, Request, StatusCode, Version};
use tokio::{
    fs::File,
    io::AsyncReadExt,
    net::TcpStream,
    sync::Mutex,
};

const STORAGE_CHUNK_SIZE: usize = 64 * 1024;
const MEDIA_KEY_PREFIX: &str = "media/";

pub type StorageBody = Pin<Box<dyn Stream<Item = Result<Bytes, std::io::Error>> + Send>>;

pub struct StorageResponse {
    pub status: StatusCode,
    pub headers: http::HeaderMap,
    pub body: StorageBody,
}

pub trait StorageClient: Clone + Send + Sync + 'static {
    fn put_file(
        &self,
        content_type: &str,
        path: &Path,
    ) -> impl Future<Output = Result<String, String>> + Send;

    fn get(
        &self,
        key: &str,
        range: Option<&str>,
        head: bool,
    ) -> impl Future<Output = Result<StorageResponse, String>> + Send;
}

#[derive(Clone)]
pub struct H2cStorageClient {
    sender: std::sync::Arc<Mutex<SendRequest<Bytes>>>,
}

impl H2cStorageClient {
    pub async fn connect(address: SocketAddr) -> Result<Self, Box<dyn Error + Send + Sync>> {
        if !address.ip().is_loopback() {
            return Err(format!("storage h2c address must be loopback: {address}").into());
        }
        let stream = TcpStream::connect(address).await?;
        let (sender, connection) = client::handshake(stream).await?;
        tokio::spawn(async move {
            if let Err(error) = connection.await {
                eprintln!("storage h2c connection ended: {error}");
            }
        });
        Ok(Self {
            sender: std::sync::Arc::new(Mutex::new(sender)),
        })
    }

    async fn start_request(
        &self,
        request: Request<()>,
        end_of_stream: bool,
    ) -> Result<(client::ResponseFuture, h2::SendStream<Bytes>), String> {
        let mut sender = self.sender.lock().await;
        poll_fn(|context| sender.poll_ready(context))
            .await
            .map_err(|error| error.to_string())?;
        sender
            .send_request(request, end_of_stream)
            .map_err(|error| error.to_string())
    }

    fn request(
        method: Method,
        key: &str,
        range: Option<&str>,
        content_type: Option<&str>,
        content_length: Option<u64>,
        create_only: bool,
        generate_key: bool,
    ) -> Result<Request<()>, String> {
        let mut builder = Request::builder()
            .version(Version::HTTP_2)
            .method(method)
            .uri(format!("http://storage.internal/objects/{key}"));
        if let Some(range) = range {
            builder = builder.header(header::RANGE, range);
        }
        if let Some(content_type) = content_type {
            builder = builder.header(header::CONTENT_TYPE, content_type);
        }
        if let Some(content_length) = content_length {
            builder = builder.header(header::CONTENT_LENGTH, content_length);
        }
        if create_only {
            builder = builder.header(header::IF_NONE_MATCH, "*");
        }
        if generate_key {
            builder = builder.header("Object-Key-Mode", "sha256");
        }
        builder.body(()).map_err(|error| error.to_string())
    }
}

impl StorageClient for H2cStorageClient {
    async fn put_file(
        &self,
        content_type: &str,
        path: &Path,
    ) -> Result<String, String> {
        let mut file = File::open(path).await.map_err(|error| error.to_string())?;
        let length = file
            .metadata()
            .await
            .map_err(|error| error.to_string())?
            .len();
        let request = Self::request(
            Method::PUT,
            MEDIA_KEY_PREFIX,
            None,
            Some(content_type),
            Some(length),
            false,
            true,
        )?;
        let (response, mut send) = self.start_request(request, false).await?;
        let upload_result = async {
            let mut sent = 0_u64;
            let mut buffer = vec![0_u8; STORAGE_CHUNK_SIZE];
            loop {
                let count = file.read(&mut buffer).await.map_err(|error| error.to_string())?;
                if count == 0 {
                    break;
                }
                let next_sent = sent
                    .checked_add(count as u64)
                    .ok_or_else(|| "media file size overflow".to_owned())?;
                if next_sent > length {
                    return Err(format!("media file changed while uploading: {}", path.display()));
                }
                send_data(&mut send, Bytes::copy_from_slice(&buffer[..count])).await?;
                sent = next_sent;
            }
            if sent != length {
                return Err(format!("media file changed while uploading: {}", path.display()));
            }
            send.send_data(Bytes::new(), true)
                .map_err(|error| error.to_string())?;
            Ok::<(), String>(())
        }
        .await;

        if let Err(upload_error) = upload_result {
            drop(send);
            if let Ok(response) = response.await {
                return Err(format!(
                    "storage upload failed with HTTP {} while sending media",
                    response.status()
                ));
            }
            return Err(upload_error);
        }

        let response = response.await.map_err(|error| error.to_string())?;
        if response.status() != StatusCode::OK {
            return Err(format!("storage upload failed with HTTP {}", response.status()));
        }
        extract_generated_media_key(response.headers())
    }

    async fn get(
        &self,
        key: &str,
        range: Option<&str>,
        head: bool,
    ) -> Result<StorageResponse, String> {
        let method = if head { Method::HEAD } else { Method::GET };
        let request = Self::request(method, key, range, None, None, false, false)?;
        let (response, _send) = self.start_request(request, true).await?;
        let response = response.await.map_err(|error| error.to_string())?;
        let status = response.status();
        let headers = response.headers().clone();
        let body = if head {
            Box::pin(stream::empty()) as StorageBody
        } else {
            Box::pin(stream::unfold(response.into_body(), |mut body| async move {
                match body.data().await {
                    Some(Ok(chunk)) => {
                        let length = chunk.len();
                        let result = body
                            .flow_control()
                            .release_capacity(length)
                            .map_err(|error| std::io::Error::other(error.to_string()))
                            .map(|()| chunk);
                        Some((result, body))
                    }
                    Some(Err(error)) => Some((Err(std::io::Error::other(error.to_string())), body)),
                    None => None,
                }
            })) as StorageBody
        };
        Ok(StorageResponse { status, headers, body })
    }
}

async fn send_data(send: &mut h2::SendStream<Bytes>, mut chunk: Bytes) -> Result<(), String> {
    while !chunk.is_empty() {
        send.reserve_capacity(chunk.len().min(STORAGE_CHUNK_SIZE));
        let capacity = poll_fn(|context| send.poll_capacity(context))
            .await
            .ok_or_else(|| "storage upload stream closed".to_owned())?
            .map_err(|error| error.to_string())?;
        if capacity == 0 {
            continue;
        }
        let amount = capacity.min(chunk.len());
        send.send_data(chunk.split_to(amount), false)
            .map_err(|error| error.to_string())?;
    }
    Ok(())
}

fn extract_generated_media_key(headers: &http::HeaderMap) -> Result<String, String> {
    let mut values = headers.get_all("Object-Name").iter();
    let value = values
        .next()
        .ok_or_else(|| "storage upload response is missing Object-Name".to_owned())?;
    if values.next().is_some() {
        return Err("storage upload response has duplicate Object-Name headers".to_owned());
    }
    let name = value
        .to_str()
        .map_err(|_| "storage upload response has malformed Object-Name".to_owned())?;
    let digest = name;
    if digest.len() != 64
        || !digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(format!("storage upload response has invalid Object-Name: {name:?}"));
    }
    Ok(format!("{MEDIA_KEY_PREFIX}{digest}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_generated_media_key_requires_exactly_one_valid_object_name() {
        let mut headers = http::HeaderMap::new();
        assert!(extract_generated_media_key(&headers).is_err());

        headers.append("Object-Name", "a".to_owned().parse().unwrap());
        headers.append("Object-Name", "b".to_owned().parse().unwrap());
        assert!(extract_generated_media_key(&headers).is_err());

        headers.clear();
        headers.insert("Object-Name", http::HeaderValue::from_bytes(b"\xff").unwrap());
        assert!(extract_generated_media_key(&headers).is_err());

        headers.insert("Object-Name", "A".repeat(64).parse().unwrap());
        assert!(extract_generated_media_key(&headers).is_err());

        headers.insert("Object-Name", "a".repeat(63).parse().unwrap());
        assert!(extract_generated_media_key(&headers).is_err());

        headers.insert("Object-Name", "a".repeat(64).parse().unwrap());
        assert_eq!(
            extract_generated_media_key(&headers).unwrap(),
            format!("{MEDIA_KEY_PREFIX}{}", "a".repeat(64))
        );
    }
}
