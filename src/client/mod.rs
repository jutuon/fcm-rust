pub(crate) mod response;

use std::path::Path;
use std::time::Duration;

use crate::client::response::{FcmResponse, RetryAfter};
use crate::{Message, MessageWrapper};
use reqwest::header::RETRY_AFTER;
use yup_oauth2::authenticator::{Authenticator, DefaultHyperClient, HyperClientBuilder};
use yup_oauth2::hyper::client::HttpConnector;
use yup_oauth2::hyper_rustls::HttpsConnector;
use yup_oauth2::ServiceAccountAuthenticator;

use self::response::FcmHttpResponseCode;


#[derive(thiserror::Error, Debug)]
pub enum FcmClientError {
    #[error("Reqwest error: {0}")]
    Reqwest(#[from] reqwest::Error),
    #[error("Service account key reading failed: {0}")]
    ServiceAccountKeyReadingFailed(std::io::Error),
    #[error("OAuth error: {0}")]
    OauthError(#[from] yup_oauth2::Error),
    #[error("Access token is missing")]
    AccessTokenIsMissing,
    #[error("Authenticator creation failed: {0}")]
    AuthenticatorCreatingFailed(std::io::Error),
    #[error("Service account key JSON does not contain project ID")]
    MissingProjectId,
}

impl FcmClientError {
    /// If this is `true` then most likely current service key is invalid.
    pub fn is_token_missing_even_if_server_requests_completed(&self) -> bool {
        matches!(
            self,
            FcmClientError::AccessTokenIsMissing |
            FcmClientError::OauthError(yup_oauth2::Error::AuthError(_))
        )
    }
}

/// An async client for sending the notification payload.
pub struct FcmClient {
    http_client: reqwest::Client,
    authenticator: Authenticator<HttpsConnector<HttpConnector>>,
    project_id: String,
}

impl FcmClient {
    /// Google recommends at least 10 minute timeout for FCM requests.
    /// https://firebase.google.com/docs/cloud-messaging/scale-fcm#timeouts
    pub async fn new(
        service_account_key_json_path: impl AsRef<Path>,
        token_cache_json_path: Option<impl AsRef<Path>>,
        fcm_request_timeout: Option<Duration>,
    ) -> Result<FcmClient, FcmClientError> {
        let builder = reqwest::ClientBuilder::new();
        let builder = if let Some(timeout) = fcm_request_timeout {
            builder.timeout(timeout)
        } else {
            builder
        };
        let http_client = builder.build()?;

        let key = yup_oauth2::read_service_account_key(service_account_key_json_path.as_ref())
            .await
            .map_err(FcmClientError::ServiceAccountKeyReadingFailed)?;
        let oauth_client = DefaultHyperClient.build_hyper_client()
            .map_err(FcmClientError::OauthError)?;
        let builder = ServiceAccountAuthenticator::with_client(key.clone(), oauth_client);
        let builder = if let Some(path) = token_cache_json_path {
            builder.persist_tokens_to_disk(path.as_ref())
        } else {
            builder
        };
        let authenticator = builder.build()
            .await
            .map_err(FcmClientError::AuthenticatorCreatingFailed)?;

        let project_id = key.project_id
            .ok_or(FcmClientError::MissingProjectId)?;

        Ok(FcmClient {
            http_client,
            authenticator,
            project_id,
        })
    }

    pub async fn send(&self, message: Message) -> Result<FcmResponse, FcmClientError> {
        let scopes = ["https://www.googleapis.com/auth/firebase.messaging"];
        let auth_token = self.authenticator.token(&scopes).await?;
        let auth_token = auth_token.token()
            .ok_or(FcmClientError::AccessTokenIsMissing)?;

        // https://firebase.google.com/docs/reference/fcm/rest/v1/projects.messages/send
        let url = format!("https://fcm.googleapis.com/v1/projects/{}/messages:send", self.project_id);

        let request = self
            .http_client
            .post(&url)
            .bearer_auth(auth_token)
            .json(&MessageWrapper::new(message.finalize()))
            .build()?;

        let response = self.http_client.execute(request).await?;
        let response_status: FcmHttpResponseCode = response.status().as_u16().into();
        let retry_after = response
            .headers()
            .get(RETRY_AFTER)
            .and_then(|ra| ra.to_str().ok())
            .and_then(|ra| ra.parse::<RetryAfter>().ok());
        let response_json_object = response.json::<serde_json::Map<String, serde_json::Value>>().await
            .ok()
            .unwrap_or_default();

        Ok(FcmResponse::new(
            response_status,
            response_json_object,
            retry_after,
        ))
    }
}
