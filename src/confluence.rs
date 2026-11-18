use anyhow::{Context, Result, bail};
use percent_encoding::percent_decode_str;
use reqwest::header::{ACCEPT, AUTHORIZATION, CONTENT_LENGTH, HeaderMap, HeaderValue};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::time::Duration;
use url::Url;

use crate::{config::Config, hash::sha256_hex, markdown};

pub(crate) const MAX_IMAGE_BYTES: usize = 5 * 1024 * 1024;
const IMAGE_ACCEPT: &str = "image/png, image/jpeg, image/gif, image/webp";

pub struct Image {
    pub data: Vec<u8>,
    pub mime_type: &'static str,
}

#[derive(Clone)]
pub struct ConfluenceClient {
    base_url: Url,
    http: reqwest::Client,
    headers: HeaderMap,
    timeout: Duration,
    #[cfg(test)]
    mock: Option<std::sync::Arc<tests::MockState>>,
}

#[derive(Serialize)]
pub struct Page {
    pub id: String,
    pub title: String,
    pub space_key: String,
    pub version: u64,
    pub url: String,
    pub markdown: String,
}

#[derive(Serialize)]
pub struct PageSummary {
    pub id: String,
    pub title: String,
    pub space_key: String,
    pub url: String,
}

#[derive(Serialize)]
pub struct SearchResult {
    pub results: Vec<PageSummary>,
    pub start: u64,
    pub limit: u32,
    pub next_start: Option<u64>,
}

#[derive(Deserialize)]
struct Content {
    id: String,
    title: String,
    #[serde(rename = "type")]
    kind: String,
    space: Space,
    version: Option<Version>,
    body: Option<Body>,
    #[serde(rename = "_links", default)]
    links: Links,
}

#[derive(Deserialize)]
struct Space {
    key: String,
}

#[derive(Deserialize)]
struct Version {
    number: u64,
}

#[derive(Deserialize)]
struct Body {
    view: Option<View>,
}

#[derive(Deserialize)]
struct View {
    value: String,
}

#[derive(Default, Deserialize)]
struct Links {
    webui: Option<String>,
    next: Option<String>,
}

#[derive(Deserialize)]
struct ContentList {
    results: Vec<Content>,
    start: u64,
    limit: u32,
    #[serde(rename = "_links", default)]
    links: Links,
}

#[derive(Debug, PartialEq)]
enum PageTarget {
    Id(String),
    Title { space: String, title: String },
}

impl ConfluenceClient {
    pub fn new(config: Config) -> Result<Self> {
        Self::with_timeout(config, Duration::from_secs(30))
    }

    fn with_timeout(config: Config, timeout: Duration) -> Result<Self> {
        let mut headers = HeaderMap::new();
        headers.insert(AUTHORIZATION, config.authorization);
        headers.insert(ACCEPT, HeaderValue::from_static("application/json"));
        let http = reqwest::Client::builder()
            .user_agent(concat!("conf-mcp/", env!("CARGO_PKG_VERSION")))
            .redirect(reqwest::redirect::Policy::none())
            .timeout(timeout)
            .build()
            .context("Не удалось создать HTTP-клиент")?;
        Ok(Self {
            base_url: config.base_url,
            http,
            headers,
            timeout,
            #[cfg(test)]
            mock: None,
        })
    }

    async fn get<T: DeserializeOwned>(&self, path: &str, query: &[(&str, &str)]) -> Result<T> {
        let url = self
            .base_url
            .join(path)
            .context("Некорректный путь REST API")?;
        let request = self
            .http
            .get(url)
            .headers(self.headers.clone())
            .timeout(self.timeout)
            .query(query)
            .build()
            .context("Не удалось сформировать HTTP-запрос Confluence")?;
        let response = self.execute(request).await?;
        check_status(&response)?;
        response.json().await.map_err(|error| {
            if error.is_timeout() {
                anyhow::anyhow!("Истёк таймаут чтения ответа Confluence")
            } else {
                anyhow::anyhow!(
                    "Confluence вернул некорректный JSON или неожиданную структуру ответа"
                )
            }
        })
    }

    async fn execute(&self, request: reqwest::Request) -> Result<reqwest::Response> {
        #[cfg(test)]
        if let Some(mock) = &self.mock {
            return mock.execute(request).await;
        }
        self.http.execute(request).await.map_err(http_error)
    }

    pub async fn get_image(&self, url: &str) -> Result<Image> {
        let url = image_url(url, &self.base_url)?;
        let mut headers = self.headers.clone();
        headers.insert(ACCEPT, HeaderValue::from_static(IMAGE_ACCEPT));
        let request = self
            .http
            .get(url)
            .headers(headers)
            .timeout(self.timeout)
            .build()
            .context("Не удалось сформировать запрос изображения Confluence")?;
        tokio::time::timeout(self.timeout, async {
            read_image(self.execute(request).await?).await
        })
        .await
        .context("Истёк таймаут загрузки изображения Confluence")?
    }

    pub fn image_cache_key(&self, input: &str) -> Result<String> {
        let url = image_url(input, &self.base_url)?;
        Ok(sha256_hex(&[
            self.headers[AUTHORIZATION].as_bytes(),
            &[0],
            url.as_str().as_bytes(),
        ]))
    }

    pub async fn get_page(&self, page: &str) -> Result<Page> {
        let expand = "space,body.view,version";
        let content: Content = match page_target(page, &self.base_url)? {
            PageTarget::Id(id) => {
                self.get(&format!("rest/api/content/{id}"), &[("expand", expand)])
                    .await?
            }
            PageTarget::Title { space, title } => {
                let mut list: ContentList = self
                    .get(
                        "rest/api/content",
                        &[
                            ("type", "page"),
                            ("status", "current"),
                            ("spaceKey", &space),
                            ("title", &title),
                            ("expand", expand),
                            ("limit", "2"),
                        ],
                    )
                    .await?;
                if list.results.is_empty() {
                    bail!("Страница не найдена или недоступна текущему пользователю");
                }
                if list.results.len() != 1 || list.links.next.is_some() {
                    bail!("Найдено несколько страниц с таким названием; используйте числовой ID");
                }
                list.results.remove(0)
            }
        };
        ensure_page(&content)?;
        let url = self.page_url(&content)?;
        let html = content
            .body
            .and_then(|body| body.view)
            .context("В ответе Confluence отсутствует body.view")?;
        let version = content
            .version
            .context("В ответе Confluence отсутствует версия страницы")?;
        Ok(Page {
            id: content.id,
            title: content.title,
            space_key: content.space.key,
            version: version.number,
            markdown: markdown::convert(&html.value, &url)?,
            url: url.into(),
        })
    }

    pub async fn search_pages(&self, cql: &str, start: u64, limit: u32) -> Result<SearchResult> {
        if !(1..=100).contains(&limit) {
            bail!("limit должен быть в диапазоне от 1 до 100");
        }
        let list: ContentList = self
            .get(
                "rest/api/content/search",
                &[
                    ("cql", cql),
                    ("start", &start.to_string()),
                    ("limit", &limit.to_string()),
                    ("expand", "space"),
                ],
            )
            .await?;
        let next_start = list
            .links
            .next
            .as_deref()
            .map(|next| -> Result<u64> {
                let url = self
                    .base_url
                    .join(next)
                    .context("Некорректная ссылка пагинации Confluence")?;
                let next = url
                    .query_pairs()
                    .find(|(key, _)| key == "start")
                    .context("В ссылке пагинации Confluence отсутствует start")?
                    .1
                    .parse::<u64>()
                    .context("Некорректный start в ссылке пагинации Confluence")?;
                if next <= list.start {
                    bail!("Ссылка пагинации Confluence не продвигает start");
                }
                Ok(next)
            })
            .transpose()?;
        let results = list
            .results
            .into_iter()
            .map(|content| {
                ensure_page(&content)?;
                Ok(PageSummary {
                    url: self.page_url(&content)?.into(),
                    id: content.id,
                    title: content.title,
                    space_key: content.space.key,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(SearchResult {
            results,
            start: list.start,
            limit: list.limit,
            next_start,
        })
    }

    fn page_url(&self, content: &Content) -> Result<Url> {
        if let Some(webui) = &content.links.webui {
            // В _links.webui Confluence часто опускает context path приложения.
            let link = if webui.starts_with('/')
                && !webui.starts_with("//")
                && !webui.starts_with(self.base_url.path())
            {
                webui.trim_start_matches('/')
            } else {
                webui
            };
            let url = self
                .base_url
                .join(link)
                .context("Некорректная ссылка страницы в ответе Confluence")?;
            if url.origin() != self.base_url.origin() {
                bail!("Confluence вернул ссылку страницы другого экземпляра");
            }
            Ok(url)
        } else {
            let mut url = self.base_url.join("pages/viewpage.action")?;
            url.query_pairs_mut().append_pair("pageId", &content.id);
            Ok(url)
        }
    }
}

fn check_status(response: &reqwest::Response) -> Result<()> {
    let status = response.status();
    if !status.is_success() {
        let explanation = match status.as_u16() {
            400 => "проверьте параметры запроса и синтаксис CQL",
            401 => "проверьте персональный токен Confluence",
            403 => "недостаточно прав для доступа к Confluence",
            404 => "запрошенный объект не найден или недоступен текущему пользователю",
            429 => "превышен лимит запросов, повторите запрос позже",
            300..=399 => "перенаправление отключено; проверьте base_url и прямую ссылку Confluence",
            _ => "ошибка сервера Confluence",
        };
        bail!("Confluence HTTP {}: {explanation}", status.as_u16());
    }
    Ok(())
}

fn image_url(input: &str, base_url: &Url) -> Result<Url> {
    let mut url =
        Url::parse(input.trim()).context("Укажите абсолютную ссылку на изображение Confluence")?;
    if url.origin() != base_url.origin() || !url.username().is_empty() || url.password().is_some() {
        bail!("Ссылка должна принадлежать настроенному экземпляру Confluence");
    }
    if !url.path().starts_with(base_url.path()) {
        bail!("Ссылка находится вне context path настроенного Confluence");
    }
    // Некоторые прокси декодируют разделители до нормализации пути.
    let decoded_path = percent_decode_str(url.path())
        .decode_utf8()
        .context("Ссылка изображения содержит некорректный UTF-8")?;
    if decoded_path.contains('\\')
        || decoded_path
            .split('/')
            .any(|part| matches!(part, "." | ".."))
    {
        bail!("Ссылка изображения содержит недопустимые сегменты пути");
    }
    url.set_fragment(None);
    Ok(url)
}

async fn read_image(mut response: reqwest::Response) -> Result<Image> {
    check_status(&response)?;
    let declared_length = response
        .headers()
        .get(CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok());
    if declared_length.is_some_and(|length| length > MAX_IMAGE_BYTES as u64) {
        bail!("Изображение превышает лимит 5 МиБ");
    }
    let mut data = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(http_error)? {
        if chunk.len() > MAX_IMAGE_BYTES - data.len() {
            bail!("Изображение превышает лимит 5 МиБ");
        }
        data.extend_from_slice(&chunk);
    }
    let mime_type = image_mime(&data)
        .context("Неподдерживаемое содержимое: ожидается изображение PNG, JPEG, GIF или WebP")?;
    Ok(Image { data, mime_type })
}

pub(crate) fn image_mime(data: &[u8]) -> Option<&'static str> {
    if data.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if data.starts_with(b"\xff\xd8\xff") {
        Some("image/jpeg")
    } else if data.starts_with(b"GIF87a") || data.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if data.starts_with(b"RIFF") && data.get(8..12) == Some(b"WEBP") {
        Some("image/webp")
    } else {
        None
    }
}

fn http_error(error: reqwest::Error) -> anyhow::Error {
    // Ответы сервера и URL запроса могут содержать секреты, поэтому не включаем их в ошибку.
    if error.is_timeout() {
        anyhow::anyhow!("Истёк таймаут запроса к Confluence")
    } else if error.is_connect() {
        anyhow::anyhow!(
            "Не удалось подключиться к Confluence; проверьте сеть, base_url и сертификат TLS"
        )
    } else {
        anyhow::anyhow!("Не удалось выполнить HTTP-запрос к Confluence")
    }
}

fn ensure_page(content: &Content) -> Result<()> {
    if content.kind != "page" {
        bail!("Запрошенный объект Confluence не является страницей");
    }
    Ok(())
}

fn page_target(page: &str, base_url: &Url) -> Result<PageTarget> {
    let page = page.trim();
    if !page.is_empty() && page.bytes().all(|byte| byte.is_ascii_digit()) {
        return Ok(PageTarget::Id(page.to_owned()));
    }
    let url = Url::parse(page)
        .context("Укажите числовой ID или абсолютную ссылку страницы Confluence")?;
    if url.origin() != base_url.origin() || !url.username().is_empty() || url.password().is_some() {
        bail!("Ссылка должна принадлежать настроенному экземпляру Confluence");
    }
    let path = url
        .path()
        .strip_prefix(base_url.path())
        .context("Ссылка находится вне context path настроенного Confluence")?;
    if path == "pages/viewpage.action" {
        let ids = url
            .query_pairs()
            .filter(|(key, _)| key == "pageId")
            .map(|(_, value)| value.into_owned())
            .collect::<Vec<_>>();
        if ids.len() == 1 && !ids[0].is_empty() && ids[0].bytes().all(|byte| byte.is_ascii_digit())
        {
            return Ok(PageTarget::Id(ids[0].clone()));
        }
        bail!("В ссылке должен быть ровно один числовой параметр pageId");
    }
    if let Some((space, title)) = path
        .strip_prefix("display/")
        .and_then(|rest| rest.split_once('/'))
    {
        let decode = |value: &str| -> Result<String> {
            Ok(percent_decode_str(&value.replace('+', " "))
                .decode_utf8()
                .context("Ссылка содержит некорректный UTF-8")?
                .into_owned())
        };
        let space = decode(space)?;
        let title = decode(title)?;
        if !space.is_empty() && !title.is_empty() {
            return Ok(PageTarget::Title { space, title });
        }
    }
    bail!(
        "Поддерживаются ссылки viewpage.action?pageId=… и /display/SPACE/Title; короткие /x/… не поддерживаются"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    use bytes::Bytes;
    use http::StatusCode;
    use serde_json::{Value, json};
    use std::collections::{HashMap, VecDeque};
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::{Arc, Mutex};
    use std::task::{Context as TaskContext, Poll};

    #[derive(Clone)]
    pub(super) struct MockState {
        status: StatusCode,
        body: Vec<u8>,
        headers: HeaderMap,
        delay: Duration,
        body_delay: Duration,
        body_error: bool,
        requests: Arc<Mutex<Vec<(String, String, String)>>>,
    }

    struct MockBody {
        chunks: VecDeque<Bytes>,
        delay: Pin<Box<tokio::time::Sleep>>,
        fail: bool,
    }

    impl http_body::Body for MockBody {
        type Data = Bytes;
        type Error = std::io::Error;

        fn poll_frame(
            mut self: Pin<&mut Self>,
            cx: &mut TaskContext<'_>,
        ) -> Poll<Option<Result<http_body::Frame<Bytes>, Self::Error>>> {
            if self.delay.as_mut().poll(cx).is_pending() {
                return Poll::Pending;
            }
            if let Some(chunk) = self.chunks.pop_front() {
                return Poll::Ready(Some(Ok(http_body::Frame::data(chunk))));
            }
            if self.fail {
                self.fail = false;
                return Poll::Ready(Some(Err(std::io::Error::other("test-secret body failure"))));
            }
            Poll::Ready(None)
        }
    }

    struct Mock {
        base_url: String,
        state: Arc<MockState>,
    }

    impl MockState {
        pub(super) async fn execute(&self, request: reqwest::Request) -> Result<reqwest::Response> {
            assert_eq!(request.method(), "GET");
            self.requests.lock().unwrap().push((
                request.url().as_str().to_owned(),
                request.headers()[AUTHORIZATION]
                    .to_str()
                    .unwrap()
                    .to_owned(),
                request.headers()[ACCEPT].to_str().unwrap().to_owned(),
            ));
            tokio::time::timeout(
                request.timeout().copied().unwrap(),
                tokio::time::sleep(self.delay),
            )
            .await
            .context("Истёк таймаут запроса к Confluence")?;
            // Поток без size_hint проверяет лимит и для ответов без Content-Length.
            let body = MockBody {
                chunks: self
                    .body
                    .chunks(64 * 1024)
                    .map(Bytes::copy_from_slice)
                    .collect(),
                delay: Box::pin(tokio::time::sleep(self.body_delay)),
                fail: self.body_error,
            };
            let mut response = http::Response::builder()
                .status(self.status)
                .body(reqwest::Body::wrap(body))
                .unwrap();
            *response.headers_mut() = self.headers.clone();
            Ok(response.into())
        }
    }

    impl Mock {
        async fn new(status: u16, body: impl Into<String>, delay: Duration) -> Self {
            let mut headers = HeaderMap::new();
            headers.insert("content-type", HeaderValue::from_static("application/json"));
            headers.insert(
                "location",
                HeaderValue::from_static("http://127.0.0.1:9/must-not-follow"),
            );
            let state = MockState {
                status: StatusCode::from_u16(status).unwrap(),
                body: body.into().into_bytes(),
                headers,
                delay,
                body_delay: Duration::ZERO,
                body_error: false,
                requests: Arc::default(),
            };
            Self {
                base_url: "https://wiki.test/confluence".into(),
                state: Arc::new(state),
            }
        }

        async fn json(value: Value) -> Self {
            Self::new(200, value.to_string(), Duration::ZERO).await
        }

        async fn binary(body: Vec<u8>) -> Self {
            let mut mock = Self::new(200, "", Duration::ZERO).await;
            let state = Arc::get_mut(&mut mock.state).unwrap();
            state.body = body;
            state.headers.remove("content-type");
            mock
        }

        fn client(&self) -> ConfluenceClient {
            let mut client =
                ConfluenceClient::new(Config::new(&self.base_url, "test-secret").unwrap()).unwrap();
            client.mock = Some(self.state.clone());
            client
        }

        fn request(&self) -> (String, HashMap<String, String>) {
            self.request_with_accept("application/json")
        }

        fn request_with_accept(&self, expected_accept: &str) -> (String, HashMap<String, String>) {
            let requests = self.state.requests.lock().unwrap();
            assert_eq!(requests.len(), 1);
            let (uri, authorization, accept) = &requests[0];
            assert_eq!(authorization, "Bearer test-secret");
            assert_eq!(accept, expected_accept);
            let url = Url::parse(uri).unwrap();
            (
                url.path().to_owned(),
                url.query_pairs()
                    .map(|(k, v)| (k.into_owned(), v.into_owned()))
                    .collect(),
            )
        }
    }

    fn content() -> Value {
        json!({
            "id": "42", "type": "page", "title": "Пример страницы", "space": {"key": "DEV"},
            "version": {"number": 7}, "body": {"view": {"value": "<h2>Привет</h2><p><a href='/confluence/pages/viewpage.action?pageId=43'>Next</a></p>"}},
            "_links": {"webui": "/display/DEV/Page"}
        })
    }

    fn list(results: Vec<Value>) -> Value {
        json!({"results": results, "start": 0, "limit": 25, "_links": {}})
    }

    fn png() -> Vec<u8> {
        STANDARD.decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=").unwrap()
    }

    #[test]
    fn image_cache_keys_include_version_and_authorization_and_validate_url() {
        let client = ConfluenceClient::new(
            Config::new("https://wiki.test/confluence", "first-secret").unwrap(),
        )
        .unwrap();
        let other = ConfluenceClient::new(
            Config::new("https://wiki.test/confluence", "second-secret").unwrap(),
        )
        .unwrap();
        let url = "https://wiki.test/confluence/download/attachments/42/image.png?version=1&api=v2";
        let key = client.image_cache_key(url).unwrap();
        assert_eq!(key.len(), 64);
        assert_eq!(
            key,
            client.image_cache_key(&format!(" {url}#preview ")).unwrap()
        );
        assert_ne!(
            key,
            client
                .image_cache_key(&url.replace("version=1", "version=2"))
                .unwrap()
        );
        assert_ne!(key, other.image_cache_key(url).unwrap());
        assert!(
            client
                .image_cache_key("https://other.test/image.png")
                .is_err()
        );
        assert!(
            client
                .image_cache_key("https://wiki.test/outside/image.png")
                .is_err()
        );
    }

    #[tokio::test]
    async fn image_download_preserves_bytes_auth_context_and_version_query() {
        let mock = Mock::binary(png()).await;
        let url = format!(
            "{}/download/attachments/42/%D0%A1%D1%85%D0%B5%D0%BC%D0%B0.png?version=2&api=v2#preview",
            mock.base_url
        );
        let image = mock.client().get_image(&url).await.unwrap();
        assert_eq!(image.mime_type, "image/png");
        assert_eq!(image.data, png());
        let (path, query) = mock.request_with_accept(IMAGE_ACCEPT);
        assert_eq!(
            path,
            "/confluence/download/attachments/42/%D0%A1%D1%85%D0%B5%D0%BC%D0%B0.png"
        );
        assert_eq!(query["version"], "2");
        assert_eq!(query["api"], "v2");
        assert!(!mock.state.requests.lock().unwrap()[0].0.contains('#'));
    }

    #[tokio::test]
    async fn invalid_image_urls_are_rejected_before_sending_pat() {
        let mock = Mock::binary(png()).await;
        let client = mock.client();
        for url in [
            "",
            "download/attachments/42/image.png",
            "//wiki.test/confluence/image.png",
            "https://other.test/confluence/image.png",
            "http://wiki.test/confluence/image.png",
            "https://wiki.test:8443/confluence/image.png",
            "https://user:secret@wiki.test/confluence/image.png",
            "https://wiki.test/confluence-other/image.png",
            "https://wiki.test/image.png",
            "https://wiki.test/confluence/../image.png",
            "https://wiki.test/confluence/%2e%2e%2fimage.png",
            "https://wiki.test/confluence/%5c..%5cimage.png",
            "file:///confluence/image.png",
            "data:image/png;base64,aaaa",
        ] {
            let error = client.get_image(url).await.err().unwrap().to_string();
            assert!(!error.contains("secret"), "{error}");
        }
        assert!(mock.state.requests.lock().unwrap().is_empty());
        assert!(
            image_url(
                "https://wiki.test/image.png",
                &Url::parse("https://wiki.test/").unwrap()
            )
            .is_ok()
        );
    }

    #[tokio::test]
    async fn image_formats_are_detected_from_bytes_not_filename_or_content_type() {
        for (body, mime) in [
            (png(), "image/png"),
            (b"\xff\xd8\xff\xe0binary".to_vec(), "image/jpeg"),
            (b"GIF87abinary".to_vec(), "image/gif"),
            (b"GIF89abinary".to_vec(), "image/gif"),
            (b"RIFF\x10\x00\x00\x00WEBPVP8 binary".to_vec(), "image/webp"),
        ] {
            let mut mock = Mock::binary(body.clone()).await;
            Arc::get_mut(&mut mock.state).unwrap().headers.insert(
                "content-type",
                HeaderValue::from_static("application/octet-stream"),
            );
            let image = mock
                .client()
                .get_image(&format!(
                    "{}/rest/documentConversion/latest/conversion/thumbnail/42/1",
                    mock.base_url
                ))
                .await
                .unwrap();
            assert_eq!(image.mime_type, mime);
            assert_eq!(image.data, body);
        }
        for body in [
            b"".as_slice(),
            b"<html>test-secret login</html>",
            b"<svg>test-secret</svg>",
            b"%PDF-1.7",
            b"RIFF\x10\x00\x00\x00WAVE",
            b"\x89PN",
        ] {
            let mut mock = Mock::binary(body.to_vec()).await;
            Arc::get_mut(&mut mock.state)
                .unwrap()
                .headers
                .insert("content-type", HeaderValue::from_static("image/png"));
            let error = mock
                .client()
                .get_image(&format!("{}/fake.png", mock.base_url))
                .await
                .err()
                .unwrap()
                .to_string();
            assert!(error.contains("PNG"), "{error}");
            assert!(!error.contains("test-secret"), "{error}");
        }
    }

    #[tokio::test]
    async fn image_limit_checks_declared_and_streamed_sizes() {
        let mut mock = Mock::binary(png()).await;
        let state = Arc::get_mut(&mut mock.state).unwrap();
        state
            .headers
            .insert(CONTENT_LENGTH, HeaderValue::from(MAX_IMAGE_BYTES + 1));
        state.body_error = true;
        let error = mock
            .client()
            .get_image(&format!("{}/image.png", mock.base_url))
            .await
            .err()
            .unwrap()
            .to_string();
        assert!(error.contains("5 МиБ"), "{error}");

        for declared_length in [None, Some(1)] {
            let mut body = png();
            body.resize(MAX_IMAGE_BYTES + 1, 0);
            let mut mock = Mock::binary(body).await;
            if let Some(length) = declared_length {
                Arc::get_mut(&mut mock.state)
                    .unwrap()
                    .headers
                    .insert(CONTENT_LENGTH, HeaderValue::from(length));
            }
            let error = mock
                .client()
                .get_image(&format!("{}/image.png", mock.base_url))
                .await
                .err()
                .unwrap()
                .to_string();
            assert!(error.contains("5 МиБ"), "{error}");
        }
        let mut body = png();
        body.resize(MAX_IMAGE_BYTES, 0);
        let mock = Mock::binary(body.clone()).await;
        assert_eq!(
            mock.client()
                .get_image(&format!("{}/image.png", mock.base_url))
                .await
                .unwrap()
                .data,
            body
        );
    }

    #[tokio::test]
    async fn image_http_body_errors_redirects_and_timeouts_are_sanitized() {
        for status in [401, 403, 404, 429, 500, 302, 307] {
            let mock = Mock::new(status, "test-secret in an error body", Duration::ZERO).await;
            let error = mock
                .client()
                .get_image(&format!("{}/image.png", mock.base_url))
                .await
                .err()
                .unwrap()
                .to_string();
            assert!(error.contains(&status.to_string()), "{error}");
            assert!(!error.contains("test-secret"), "{error}");
            mock.request_with_accept(IMAGE_ACCEPT);
        }
        let mut mock = Mock::binary(png()).await;
        Arc::get_mut(&mut mock.state).unwrap().body_error = true;
        let error = mock
            .client()
            .get_image(&format!("{}/image.png", mock.base_url))
            .await
            .err()
            .unwrap()
            .to_string();
        assert!(!error.contains("test-secret"), "{error}");
        for slow_body in [false, true] {
            let mut mock = Mock::binary(png()).await;
            let state = Arc::get_mut(&mut mock.state).unwrap();
            if slow_body {
                state.body_delay = Duration::from_millis(150);
            } else {
                state.delay = Duration::from_millis(150);
            }
            let mut client = mock.client();
            client.timeout = Duration::from_millis(25);
            let error = client
                .get_image(&format!("{}/image.png", mock.base_url))
                .await
                .err()
                .unwrap()
                .to_string();
            assert!(error.contains("таймаут"), "{error}");
        }
    }

    #[test]
    fn ids_and_supported_links_are_parsed() {
        let base = Url::parse("https://wiki.test/confluence/").unwrap();
        for input in [
            "42",
            " 42 ",
            "https://wiki.test/confluence/pages/viewpage.action?pageId=42#anchor",
        ] {
            assert_eq!(
                page_target(input, &base).unwrap(),
                PageTarget::Id("42".into())
            );
        }
        assert_eq!(
            page_target(
                "https://wiki.test/confluence/display/DEV/%D0%A2%D0%B5%D1%81%D1%82+C%2B%2B",
                &base
            )
            .unwrap(),
            PageTarget::Title {
                space: "DEV".into(),
                title: "Тест C++".into()
            }
        );
        for input in [
            "",
            "../42",
            "https://other.test/confluence/pages/viewpage.action?pageId=42",
            "http://wiki.test/confluence/pages/viewpage.action?pageId=42",
            "https://wiki.test:8443/confluence/pages/viewpage.action?pageId=42",
            "https://wiki.test/confluence-other/pages/viewpage.action?pageId=42",
            "https://wiki.test/pages/viewpage.action?pageId=42",
            "https://user:secret@wiki.test/confluence/pages/viewpage.action?pageId=42",
            "https://wiki.test/confluence/pages/viewpage.action?pageId=42&pageId=43",
            "https://wiki.test/confluence/pages/viewpage.action?pageId=abc",
            "https://wiki.test/confluence/display/DEV/",
            "https://wiki.test/confluence/x/abcd",
        ] {
            assert!(page_target(input, &base).is_err(), "{input}");
        }
        assert!(
            page_target(
                "https://wiki.test/pages/viewpage.action?pageId=42",
                &Url::parse("https://wiki.test/").unwrap()
            )
            .is_ok()
        );
    }

    #[tokio::test]
    async fn page_requests_bearer_body_view_and_context_path() {
        let mock = Mock::json(content()).await;
        let page = mock.client().get_page("42").await.unwrap();
        assert_eq!(
            (page.id.as_str(), page.space_key.as_str(), page.version),
            ("42", "DEV", 7)
        );
        assert_eq!(page.url, format!("{}/display/DEV/Page", mock.base_url));
        assert!(page.markdown.contains("## Привет"));
        assert!(page.markdown.contains(&format!(
            "{}/pages/viewpage.action?pageId=43",
            mock.base_url
        )));
        let (path, query) = mock.request();
        assert_eq!(path, "/confluence/rest/api/content/42");
        assert_eq!(query["expand"], "space,body.view,version");
    }

    #[tokio::test]
    async fn display_links_resolve_by_exact_space_and_title() {
        let mock = Mock::json(list(vec![content()])).await;
        mock.client()
            .get_page(&format!("{}/display/DEV/C%2B%2B+Guide", mock.base_url))
            .await
            .unwrap();
        let (path, query) = mock.request();
        assert_eq!(path, "/confluence/rest/api/content");
        assert_eq!(query["spaceKey"], "DEV");
        assert_eq!(query["title"], "C++ Guide");
        assert_eq!(query["type"], "page");
        assert_eq!(query["status"], "current");
        for results in [vec![], vec![content(), content()]] {
            let mock = Mock::json(list(results)).await;
            assert!(
                mock.client()
                    .get_page(&format!("{}/display/DEV/Page", mock.base_url))
                    .await
                    .is_err()
            );
        }
    }

    #[tokio::test]
    async fn search_uses_server_pagination_including_empty_last_page() {
        let mut body = list(vec![content()]);
        body["limit"] = json!(2);
        body["_links"]["next"] = json!("/rest/api/content/search?start=5&limit=2");
        let mock = Mock::json(body).await;
        let cql = "type = page AND (space = DEV) ORDER BY title";
        let result = mock.client().search_pages(cql, 0, 2).await.unwrap();
        assert_eq!(result.next_start, Some(5));
        assert_eq!(result.results[0].id, "42");
        let (path, query) = mock.request();
        assert_eq!(path, "/confluence/rest/api/content/search");
        assert_eq!(query["cql"], cql);
        assert_eq!(query["expand"], "space");
        assert_eq!(query["limit"], "2");
        assert_eq!(query["start"], "0");
        let mut body = list(vec![]);
        body["start"] = json!(5);
        let mock = Mock::json(body).await;
        let result = mock.client().search_pages(cql, 5, 25).await.unwrap();
        assert!(result.results.is_empty());
        assert_eq!(result.next_start, None);
        assert_eq!(result.start, 5);
        for limit in [0, 101] {
            assert!(mock.client().search_pages(cql, 0, limit).await.is_err());
        }
        assert_eq!(mock.state.requests.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn errors_and_redirects_do_not_expose_response_or_follow_location() {
        for status in [400, 401, 403, 404, 429, 500, 503, 302, 307] {
            let mock = Mock::new(status, "test-secret in an error body", Duration::ZERO).await;
            let error = mock
                .client()
                .get_page("42")
                .await
                .err()
                .unwrap()
                .to_string();
            assert!(error.contains(&status.to_string()), "{error}");
            assert!(!error.contains("test-secret"));
            mock.request();
        }
        for body in [
            "<html>test-secret login form</html>",
            "{\"id\": \"test-secret\"}",
        ] {
            let mock = Mock::new(200, body, Duration::ZERO).await;
            let error = mock
                .client()
                .get_page("42")
                .await
                .err()
                .unwrap()
                .to_string();
            assert!(error.contains("JSON"));
            assert!(!error.contains("test-secret"));
        }
    }

    #[tokio::test]
    async fn missing_body_wrong_content_type_and_timeout_are_errors() {
        for value in [
            {
                let mut value = content();
                value["body"] = json!({});
                value
            },
            {
                let mut value = content();
                value["type"] = json!("blogpost");
                value
            },
        ] {
            let mock = Mock::json(value).await;
            assert!(mock.client().get_page("42").await.is_err());
        }
        let mock = Mock::new(200, content().to_string(), Duration::from_millis(150)).await;
        let mut client = ConfluenceClient::with_timeout(
            Config::new(&mock.base_url, "test-secret").unwrap(),
            Duration::from_millis(25),
        )
        .unwrap();
        client.mock = Some(mock.state.clone());
        let error = client.get_page("42").await.err().unwrap().to_string();
        assert!(error.contains("таймаут"), "{error}");
    }

    #[test]
    fn webui_context_path_is_not_duplicated_and_fallback_uses_page_id() {
        let client =
            ConfluenceClient::new(Config::new("https://wiki.test/confluence/", "secret").unwrap())
                .unwrap();
        for webui in [
            "/display/DEV/Page",
            "/confluence/display/DEV/Page",
            "display/DEV/Page",
        ] {
            let mut value = content();
            value["_links"]["webui"] = json!(webui);
            let content = serde_json::from_value(value).unwrap();
            assert_eq!(
                client.page_url(&content).unwrap().as_str(),
                "https://wiki.test/confluence/display/DEV/Page"
            );
        }
        let mut value = content();
        value["_links"] = json!({});
        let content = serde_json::from_value(value).unwrap();
        assert_eq!(
            client.page_url(&content).unwrap().as_str(),
            "https://wiki.test/confluence/pages/viewpage.action?pageId=42"
        );
    }
}
