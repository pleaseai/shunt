use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::Serialize;

#[derive(Debug)]
pub struct UpstreamError {
    message: String,
}

impl UpstreamError {
    pub fn from_reqwest(error: reqwest::Error) -> Self {
        Self {
            message: error.to_string(),
        }
    }

    pub fn from_message(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

#[derive(Debug, Serialize)]
struct AnthropicErrorBody {
    #[serde(rename = "type")]
    kind: &'static str,
    error: AnthropicErrorDetail,
}

#[derive(Debug, Serialize)]
struct AnthropicErrorDetail {
    #[serde(rename = "type")]
    kind: &'static str,
    message: String,
}

impl IntoResponse for UpstreamError {
    fn into_response(self) -> Response {
        (
            StatusCode::BAD_GATEWAY,
            Json(AnthropicErrorBody {
                kind: "error",
                error: AnthropicErrorDetail {
                    kind: "api_error",
                    message: self.message,
                },
            }),
        )
            .into_response()
    }
}
