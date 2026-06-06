use std::sync::Arc;
use std::pin::Pin;
use std::task::{Context as StdContext, Poll};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use axum_server::accept::Accept;
use tokio_rustls::server::TlsStream;
use futures::future::BoxFuture;
use tower::Service;
use axum::http::Request;

/// A custom stream that carries the peer certificates
pub struct MtlsStream<S> {
    pub inner: TlsStream<S>,
    pub certs: Arc<Vec<Vec<u8>>>,
}

impl<S: AsyncRead + AsyncWrite + Unpin> AsyncRead for MtlsStream<S> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut StdContext<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin> AsyncWrite for MtlsStream<S> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut StdContext<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut StdContext<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut StdContext<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

/// Helper service to inject certs into request extensions
#[derive(Clone)]
pub struct ConnectionService<S> {
    pub inner: S,
    pub certs: Arc<Vec<Vec<u8>>>,
}

impl<S, ReqBody> Service<Request<ReqBody>> for ConnectionService<S>
where
    S: Service<Request<ReqBody>> + Clone,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    fn poll_ready(&mut self, cx: &mut StdContext<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, mut req: Request<ReqBody>) -> Self::Future {
        req.extensions_mut().insert(self.certs.clone());
        self.inner.call(req)
    }
}

/// A custom acceptor that extracts client certificates after the TLS handshake
#[derive(Clone)]
pub struct MtlsAcceptor {
    config: Arc<rustls::ServerConfig>,
}

impl MtlsAcceptor {
    pub fn new(config: Arc<rustls::ServerConfig>) -> Self {
        Self { config }
    }
}

impl<S, T> Accept<S, T> for MtlsAcceptor
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    T: Service<Request<axum::body::Body>> + Clone + Send + 'static,
{
    type Stream = MtlsStream<S>;
    type Service = ConnectionService<T>;
    type Future = BoxFuture<'static, std::io::Result<(Self::Stream, Self::Service)>>;

    fn accept(&self, stream: S, service: T) -> Self::Future {
        let config = self.config.clone();
        Box::pin(async move {
            let acceptor = tokio_rustls::TlsAcceptor::from(config);
            let tls_stream = acceptor.accept(stream).await?;
            
            let certs = tls_stream.get_ref().1.peer_certificates()
                .map(|c| c.iter().map(|cert| cert.as_ref().to_vec()).collect::<Vec<_>>())
                .unwrap_or_default();
            let certs = Arc::new(certs);
            
            let service = ConnectionService { inner: service, certs: certs.clone() };

            Ok((MtlsStream { inner: tls_stream, certs }, service))
        })
    }
}
