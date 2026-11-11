use anyhow::{Result, bail};
use http::{HeaderMap, StatusCode};
use std::time::Duration;
use ureq::Agent;

#[derive(Clone)]
pub struct Client {
    agent: Agent,
}

pub struct ClientBuilder {
    timeout: Duration,
}

impl ClientBuilder {
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    #[allow(dead_code)]
    pub fn no_proxy(self) -> Self {
        self
    }

    pub fn build(self) -> Result<Client> {
        let config = ureq::config::Config::builder()
            .http_status_as_error(false)
            .timeout_global(Some(self.timeout))
            .build();
        Ok(Client {
            agent: config.new_agent(),
        })
    }
}

impl Client {
    pub fn new() -> Self {
        Self::builder().build().unwrap()
    }

    pub fn builder() -> ClientBuilder {
        ClientBuilder {
            timeout: Duration::from_secs(10),
        }
    }

    pub fn get(&self, url: impl AsRef<str>) -> RequestBuilder {
        RequestBuilder {
            agent: self.agent.clone(),
            method: http::Method::GET,
            url: url.as_ref().to_string(),
            headers: Vec::new(),
            body: None,
        }
    }

    pub fn post(&self, url: impl AsRef<str>) -> RequestBuilder {
        RequestBuilder {
            agent: self.agent.clone(),
            method: http::Method::POST,
            url: url.as_ref().to_string(),
            headers: Vec::new(),
            body: None,
        }
    }

    pub fn delete(&self, url: impl AsRef<str>) -> RequestBuilder {
        RequestBuilder {
            agent: self.agent.clone(),
            method: http::Method::DELETE,
            url: url.as_ref().to_string(),
            headers: Vec::new(),
            body: None,
        }
    }
}

pub struct RequestBuilder {
    agent: Agent,
    method: http::Method,
    url: String,
    headers: Vec<(String, String)>,
    body: Option<Vec<u8>>,
}

impl RequestBuilder {
    pub fn header(mut self, key: impl Into<String>, val: impl Into<String>) -> Self {
        self.headers.push((key.into(), val.into()));
        self
    }

    pub fn body(mut self, body: impl Into<Vec<u8>>) -> Self {
        self.body = Some(body.into());
        self
    }

    pub fn json(mut self, val: &impl serde::Serialize) -> Self {
        if let Ok(bytes) = serde_json::to_vec(val) {
            self.headers.push(("Content-Type".into(), "application/json".into()));
            self.body = Some(bytes);
        }
        self
    }

    pub fn send(self) -> Result<Response> {
        let resp = match self.method {
            http::Method::GET => {
                let mut req = self.agent.get(&self.url);
                for (k, v) in self.headers {
                    req = req.header(&k, &v);
                }
                req.call()?
            }
            http::Method::POST => {
                let mut req = self.agent.post(&self.url);
                for (k, v) in self.headers {
                    req = req.header(&k, &v);
                }
                match self.body {
                    Some(b) => req.send(b)?,
                    None => req.send_empty()?,
                }
            }
            http::Method::DELETE => {
                let mut req = self.agent.delete(&self.url);
                for (k, v) in self.headers {
                    req = req.header(&k, &v);
                }
                req.call()?
            }
            _ => unimplemented!(),
        };

        Ok(Response { inner: resp })
    }
}

pub struct Response {
    inner: http::Response<ureq::Body>,
}

impl Response {
    pub fn status(&self) -> StatusCode {
        self.inner.status()
    }

    pub fn headers(&self) -> &HeaderMap {
        self.inner.headers()
    }

    #[allow(dead_code)]
    pub fn text(mut self) -> Result<String> {
        Ok(self.inner.body_mut().read_to_string()?)
    }

    pub fn bytes(mut self) -> Result<Vec<u8>> {
        Ok(self.inner.body_mut().read_to_vec()?)
    }

    pub fn json<T: serde::de::DeserializeOwned>(mut self) -> Result<T> {
        Ok(self.inner.body_mut().read_json()?)
    }

    pub fn error_for_status(self) -> Result<Self> {
        if self.inner.status().is_client_error() || self.inner.status().is_server_error() {
            bail!("HTTP error: {}", self.inner.status());
        }
        Ok(self)
    }
}
