use std::{future::Future, pin::Pin};

use axum::{
    http::{HeaderMap, StatusCode, Uri},
    response::Response,
};

use crate::{routing::Route, server::AppState};

pub mod anthropic;
pub mod responses;

pub type AdapterResult = Result<(StatusCode, Response), AdapterError>;
pub type AdapterFuture<'a> = Pin<Box<dyn Future<Output = AdapterResult> + Send + 'a>>;

#[derive(Debug)]
pub struct AdapterError {
    pub message: String,
    pub response: Box<Response>,
}

pub trait Adapter {
    fn forward<'a>(
        &'a self,
        state: AppState,
        route: Route,
        uri: &'a Uri,
        headers: &'a HeaderMap,
        body: Vec<u8>,
    ) -> AdapterFuture<'a>;
}
