use std::future::Future;
use std::net::SocketAddr;

use async_trait::async_trait;
use coap_lite::{CoapRequest, CoapResponse};

#[async_trait]
pub trait RequestHandler: Send {
    async fn handle_request(&mut self, request: &mut CoapRequest<SocketAddr>) -> ResponseBehaviour;
    // async fn on_observer_added(&mut self, path: &[String], )
}

#[async_trait]
pub trait LegacyRequestHandler: Send + Sync {
    async fn handle_request(&mut self, request: CoapRequest<SocketAddr>) -> Option<CoapResponse>;
}

#[async_trait]
impl<F, R: 'static> LegacyRequestHandler for F
where
     F: Fn(CoapRequest<SocketAddr>) -> R + Send + Sync,
     R: Future<Output = Option<CoapResponse>> + Send {
    async fn handle_request(&mut self, request: CoapRequest<SocketAddr>) -> Option<CoapResponse> {
        (self)(request).await
    }
}

pub fn legacy_handler(handler: impl LegacyRequestHandler + 'static) -> impl RequestHandler {
    LegacyAdapter(Box::new(handler))
}

struct LegacyAdapter(Box<dyn LegacyRequestHandler + Send + Sync>);

#[async_trait]
impl RequestHandler for LegacyAdapter {
    async fn handle_request(&mut self, request: &mut CoapRequest<SocketAddr>) -> ResponseBehaviour {
        // clone() is required to support the legacy interface which grants ownership to the
        // handler.  We don't want that as it greatly limits our flexibility to manipulate the
        // request/response internally, even after dispatching to the handler.
        let response = self.0.handle_request(request.clone()).await;
        request.response = response;
        match response {
            Some(_) => ResponseBehaviour::Normal,
            None => ResponseBehaviour::Swallow,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResponseBehaviour {
    Normal,
    Swallow,
}

impl Default for ResponseBehaviour {
    fn default() -> Self {
        ResponseBehaviour::Normal
    }
}
