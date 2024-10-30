use std::collections::HashMap;
use std::io::Read;
use std::sync::Arc;

use anyhow::Error;
use http::Response;

use crate::HttpClient;
use crate::HttpOptions;

#[derive(Default)]
/// Blocking HTTP client for usage with [`HttpClient`].
pub struct Client {
    options: HttpOptions,
}

impl Client {
    pub fn new(options: HttpOptions) -> Self {
        Self { options }
    }

    fn agent(&self) -> Result<ureq::Agent, Error> {
        let mut builder = ureq::AgentBuilder::new();

        let connector = Arc::new(native_tls::TlsConnector::new()?);
        builder = builder.tls_connector(connector);

        builder = builder.user_agent(self.options.user_agent.as_deref().unwrap_or(concat!(
            "proxmox-sync-http-client/",
            env!("CARGO_PKG_VERSION")
        )));

        if let Some(proxy_config) = &self.options.proxy_config {
            builder = builder.proxy(ureq::Proxy::new(proxy_config.to_proxy_string()?)?);
        }

        Ok(builder.build())
    }

    fn call(req: ureq::Request) -> Result<ureq::Response, Error> {
        req.call().map_err(Into::into)
    }

    fn send<R>(req: ureq::Request, body: R) -> Result<ureq::Response, Error>
    where
        R: Read,
    {
        req.send(body).map_err(Into::into)
    }

    fn convert_response(res: &ureq::Response) -> Result<http::response::Builder, Error> {
        let mut builder = http::response::Builder::new()
            .status(http::status::StatusCode::from_u16(res.status())?);

        for header in res.headers_names() {
            if let Some(value) = res.header(&header) {
                builder = builder.header(header, value);
            }
        }

        Ok(builder)
    }

    fn add_headers(
        mut req: ureq::Request,
        content_type: Option<&str>,
        extra_headers: Option<&HashMap<String, String>>,
    ) -> ureq::Request {
        if let Some(content_type) = content_type {
            req = req.set("Content-Type", content_type);
        }

        if let Some(extra_headers) = extra_headers {
            for (header, value) in extra_headers {
                req = req.set(header, value);
            }
        }

        req
    }

    fn convert_response_to_string(res: ureq::Response) -> Result<Response<String>, Error> {
        let builder = Self::convert_response(&res)?;
        let body = res.into_string()?;
        builder.body(body).map_err(Into::into)
    }

    fn convert_response_to_vec(res: ureq::Response) -> Result<Response<Vec<u8>>, Error> {
        let builder = Self::convert_response(&res)?;
        let mut body = Vec::new();
        res.into_reader().read_to_end(&mut body)?;
        builder.body(body).map_err(Into::into)
    }

    fn convert_response_to_reader(res: ureq::Response) -> Result<Response<Box<dyn Read>>, Error> {
        let builder = Self::convert_response(&res)?;
        let reader = res.into_reader();
        let boxed: Box<dyn Read> = Box::new(reader);
        builder.body(boxed).map_err(Into::into)
    }
}

impl HttpClient<String, String> for Client {
    fn get(
        &self,
        uri: &str,
        extra_headers: Option<&HashMap<String, String>>,
    ) -> Result<Response<String>, Error> {
        let req = self.agent()?.get(uri);
        let req = Self::add_headers(req, None, extra_headers);

        Self::call(req).and_then(Self::convert_response_to_string)
    }

    fn post(
        &self,
        uri: &str,
        body: Option<String>,
        content_type: Option<&str>,
        extra_headers: Option<&HashMap<String, String>>,
    ) -> Result<Response<String>, Error> {
        let req = self.agent()?.post(uri);
        let req = Self::add_headers(req, content_type, extra_headers);

        match body {
            Some(body) => Self::send(req, body.as_bytes()),
            None => Self::call(req),
        }
        .and_then(Self::convert_response_to_string)
    }

    fn request(&self, request: http::Request<String>) -> Result<Response<String>, Error> {
        let mut req = self
            .agent()?
            .request(request.method().as_str(), &request.uri().to_string());

        let orig_headers = request.headers();

        for header in orig_headers.keys() {
            for value in orig_headers.get_all(header) {
                req = req.set(header.as_str(), value.to_str()?);
            }
        }

        Self::send(req, request.body().as_bytes()).and_then(Self::convert_response_to_string)
    }
}

impl HttpClient<&[u8], Vec<u8>> for Client {
    fn get(
        &self,
        uri: &str,
        extra_headers: Option<&HashMap<String, String>>,
    ) -> Result<Response<Vec<u8>>, Error> {
        let req = self.agent()?.get(uri);
        let req = Self::add_headers(req, None, extra_headers);

        Self::call(req).and_then(Self::convert_response_to_vec)
    }

    fn post(
        &self,
        uri: &str,
        body: Option<&[u8]>,
        content_type: Option<&str>,
        extra_headers: Option<&HashMap<String, String>>,
    ) -> Result<Response<Vec<u8>>, Error> {
        let req = self.agent()?.post(uri);
        let req = Self::add_headers(req, content_type, extra_headers);

        match body {
            Some(body) => Self::send(req, body),
            None => Self::call(req),
        }
        .and_then(Self::convert_response_to_vec)
    }

    fn request(&self, request: http::Request<&[u8]>) -> Result<Response<Vec<u8>>, Error> {
        let mut req = self
            .agent()?
            .request(request.method().as_str(), &request.uri().to_string());

        let orig_headers = request.headers();

        for header in orig_headers.keys() {
            for value in orig_headers.get_all(header) {
                req = req.set(header.as_str(), value.to_str()?);
            }
        }

        Self::send(req, *request.body()).and_then(Self::convert_response_to_vec)
    }
}

impl HttpClient<Box<dyn Read>, Box<dyn Read>> for Client {
    fn get(
        &self,
        uri: &str,
        extra_headers: Option<&HashMap<String, String>>,
    ) -> Result<Response<Box<dyn Read>>, Error> {
        let req = self.agent()?.get(uri);
        let req = Self::add_headers(req, None, extra_headers);

        Self::call(req).and_then(Self::convert_response_to_reader)
    }

    fn post(
        &self,
        uri: &str,
        body: Option<Box<dyn Read>>,
        content_type: Option<&str>,
        extra_headers: Option<&HashMap<String, String>>,
    ) -> Result<Response<Box<dyn Read>>, Error> {
        let req = self.agent()?.post(uri);
        let req = Self::add_headers(req, content_type, extra_headers);

        match body {
            Some(body) => Self::send(req, body),
            None => Self::call(req),
        }
        .and_then(Self::convert_response_to_reader)
    }

    fn request(
        &self,
        mut request: http::Request<Box<dyn Read>>,
    ) -> Result<Response<Box<dyn Read>>, Error> {
        let mut req = self
            .agent()?
            .request(request.method().as_str(), &request.uri().to_string());
        let orig_headers = request.headers();

        for header in orig_headers.keys() {
            for value in orig_headers.get_all(header) {
                req = req.set(header.as_str(), value.to_str()?);
            }
        }

        Self::send(req, Box::new(request.body_mut())).and_then(Self::convert_response_to_reader)
    }
}
