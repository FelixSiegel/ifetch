use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread::{self, JoinHandle},
};
use tiny_http::{Header, Response, Server, StatusCode};
use url::Url;

#[allow(dead_code)]
pub struct MockUpstream {
    pub port: u16,
    pub base_url: String,
    pub search_hits: Arc<AtomicUsize>,
    pub manga_hits: Arc<Mutex<HashMap<String, usize>>>,
    pub chapter_hits: Arc<AtomicUsize>,
    pub mirror_sv3_hits: Arc<AtomicUsize>,
    pub image_hits: Arc<AtomicUsize>,
    pub transient_retry_hits: Arc<AtomicUsize>,
    pub webhooks: Arc<Mutex<Vec<serde_json::Value>>>,
    pub enable_extra_chapter: Arc<AtomicBool>,
    server: Arc<Server>,
    workers: Vec<JoinHandle<()>>,
}

impl MockUpstream {
    pub fn start() -> anyhow::Result<Self> {
        let server = Arc::new(
            Server::http("127.0.0.1:0")
                .map_err(|e| anyhow::anyhow!("Failed to bind mock upstream server: {}", e))?,
        );
        let port = server
            .server_addr()
            .to_ip()
            .ok_or_else(|| anyhow::anyhow!("Mock upstream has no IP addr"))?
            .port();
        let base_url = format!("http://127.0.0.1:{}", port);

        let search_hits = Arc::new(AtomicUsize::new(0));
        let manga_hits = Arc::new(Mutex::new(HashMap::new()));
        let chapter_hits = Arc::new(AtomicUsize::new(0));
        let mirror_sv3_hits = Arc::new(AtomicUsize::new(0));
        let image_hits = Arc::new(AtomicUsize::new(0));
        let transient_retry_hits = Arc::new(AtomicUsize::new(0));
        let webhooks = Arc::new(Mutex::new(Vec::new()));
        let enable_extra_chapter = Arc::new(AtomicBool::new(false));

        let mut workers = Vec::with_capacity(4);
        for idx in 0..4 {
            let srv = Arc::clone(&server);
            let base = base_url.clone();
            let search_hits = Arc::clone(&search_hits);
            let manga_hits = Arc::clone(&manga_hits);
            let chapter_hits = Arc::clone(&chapter_hits);
            let mirror_sv3_hits = Arc::clone(&mirror_sv3_hits);
            let image_hits = Arc::clone(&image_hits);
            let transient_retry_hits = Arc::clone(&transient_retry_hits);
            let webhooks = Arc::clone(&webhooks);
            let enable_extra_chapter = Arc::clone(&enable_extra_chapter);

            let handle = thread::Builder::new()
                .name(format!("mock-upstream-{}", idx))
                .spawn(move || {
                    while let Ok(mut req) = srv.recv() {
                        let raw_url = format!("{}{}", base, req.url());
                        let parsed = match Url::parse(&raw_url) {
                            Ok(u) => u,
                            Err(_) => {
                                let _ = req.respond(
                                    Response::from_string("Bad URL").with_status_code(400),
                                );
                                continue;
                            }
                        };

                        let path = parsed.path().to_string();
                        let query: HashMap<String, String> =
                            parsed.query_pairs().into_owned().collect();

                        if path == "/webhook" && req.method().as_str() == "POST" {
                            let mut body = String::new();
                            let _ = req.as_reader().read_to_string(&mut body);
                            if let Ok(json) = serde_json::from_str::<serde_json::Value>(&body) {
                                webhooks.lock().unwrap().push(json);
                            }
                            let _ =
                                req.respond(Response::from_string("ok").with_status_code(204));
                            continue;
                        }

                        if path == "/" {
                            search_hits.fetch_add(1, Ordering::SeqCst);
                            let q = query.get("search").map(|s| s.as_str()).unwrap_or("");
                            if q == "no-match-xyz" {
                                let html = "<html><body><div id='book_list'></div></body></html>";
                                let _ = req.respond(html_resp(200, html));
                            } else if q == "direct-hit" {
                                let loc = format!("{}/manga/direct-manga.777", base);
                                let resp = Response::from_string("")
                                    .with_status_code(302)
                                    .with_header(
                                        Header::from_bytes(&b"Location"[..], loc.as_bytes())
                                            .unwrap(),
                                    );
                                let _ = req.respond(resp);
                            } else {
                                let html = format!(
                                    r#"<html><body>
                                    <div id="hot_book" class="widget">
                                        <div class="item">
                                            <div class="title"><a href="{base}/manga/cyber-blade.101">Cyber Blade &amp; &lt;Steel&gt;</a></div>
                                            <div class="wrap_img"><img data-src="{base}/img/cover_101.jpg" /></div>
                                            <div class="status">Ongoing</div>
                                        </div>
                                    </div>
                                    <div id="book_list">
                                        <div class="item">
                                            <div class="title"><a href="{base}/manga/cyber-blade.101">Cyber Blade &amp; &lt;Steel&gt;</a></div>
                                            <div class="wrap_img"><img data-src="{base}/img/cover_101.jpg" /></div>
                                            <div class="status">Ongoing</div>
                                        </div>
                                        <div class="item">
                                            <div class="title"><a href="/manga/padding-saga.202">Padding Saga</a></div>
                                            <div class="wrap_img"><img src="{base}/img/cover_202.png" /></div>
                                            <div class="status">Completed</div>
                                        </div>
                                        <!-- Duplicate item to verify deduplication -->
                                        <div class="item">
                                            <div class="title"><a href="{base}/manga/cyber-blade.101">Cyber Blade Duplicate</a></div>
                                        </div>
                                    </div>
                                    </body></html>"#
                                );
                                let _ = req.respond(html_resp(200, &html));
                            }
                            continue;
                        }

                        if path == "/latest"
                            || path.starts_with("/latest/page/")
                            || path == "/new-manga"
                            || path.starts_with("/new-manga/page/")
                        {
                            search_hits.fetch_add(1, Ordering::SeqCst);
                            let html = format!(
                                r#"<html><body>
                                <div id="book_list">
                                    <div class="item">
                                        <div class="title"><a href="{base}/manga/cyber-blade.101">Cyber Blade &amp; &lt;Steel&gt;</a></div>
                                        <div class="wrap_img"><img data-src="{base}/img/cover_101.jpg" /></div>
                                        <div class="status">Ongoing</div>
                                    </div>
                                    <div class="item">
                                        <div class="title"><a href="/manga/padding-saga.202">Padding Saga</a></div>
                                        <div class="wrap_img"><img src="{base}/img/cover_202.png" /></div>
                                        <div class="status">Completed</div>
                                    </div>
                                </div>
                                </body></html>"#
                            );
                            let _ = req.respond(html_resp(200, &html));
                            continue;
                        }

                        if let Some(manga_id) = path
                            .strip_prefix("/manga/")
                            .filter(|rest| !rest.contains('/'))
                        {
                            {
                                let mut map = manga_hits.lock().unwrap();
                                *map.entry(manga_id.to_string()).or_insert(0) += 1;
                            }

                            match manga_id {
                                "cyber-blade.101" => {
                                    let extra_ch = if enable_extra_chapter.load(Ordering::SeqCst) {
                                        format!(
                                            r#"<tr><td><div class="chapter"><a href="{base}/manga/cyber-blade.101/c11">Chapter 11: Awakening</a></div></td></tr>"#
                                        )
                                    } else {
                                        String::new()
                                    };
                                    let html = format!(
                                        r#"<html><body>
                                        <h1 class="heading">Cyber Blade &amp; &lt;Steel&gt;</h1>
                                        <div class="cover"><img src="{base}/img/cover_101.jpg" /></div>
                                        <div class="d-cell-small value status">Ongoing</div>
                                        <div class="summary"><p>A deterministic sci-fi test series with XML chars: A &amp; B &lt; C &gt; D.</p></div>
                                        <div class="genres"><a href="/genre/action">Action</a><a href="/genre/scifi">Sci-Fi</a></div>
                                        <div class="authors"><a class="author" href="/author/ada">Ada Lovelace</a><a class="author" href="/author/alan">Alan Turing</a></div>
                                        <div class="alt_name">Cyber Blade ; Blade of Steel</div>
                                        <div class="chapters">
                                            <table>
                                                {extra_ch}
                                                <tr><td><div class="chapter"><a href="{base}/manga/cyber-blade.101/c10.5">Chapter 10.5: Side Story</a></div></td></tr>
                                                <tr><td><div class="chapter"><a href="{base}/manga/cyber-blade.101/c2">Chapter 2: Mirror Fallback</a></div></td></tr>
                                                <tr><td><div class="chapter"><a href="{base}/manga/cyber-blade.101/c1">Chapter 1: Genesis</a></div></td></tr>
                                                <tr><td><div class="chapter"><a href="{base}/manga/cyber-blade.101/c1">Chapter 1: Duplicate Link</a></div></td></tr>
                                            </table>
                                        </div>
                                        </body></html>"#
                                    );
                                    let _ = req.respond(html_resp(200, &html));
                                }
                                "padding-saga.202" => {
                                    let html = format!(
                                        r#"<html><body>
                                        <h1>Padding Saga</h1>
                                        <div class="cover"><img src="{base}/img/cover_202.png" /></div>
                                        <div class="status">Completed</div>
                                        <div class="summary"><p>Tests 4-digit zero-padding upgrade (001 -> 0001).</p></div>
                                        <div class="genres"><a href="/genre/fantasy">Fantasy</a></div>
                                        <div class="authors"><a class="author" href="/author/grace">Grace Hopper</a></div>
                                        <div class="chapters">
                                            <a href="{base}/manga/padding-saga.202/c1000">Chapter 1000: Finale</a>
                                            <a href="{base}/manga/padding-saga.202/c1">Chapter 1: Beginning</a>
                                        </div>
                                        </body></html>"#
                                    );
                                    let _ = req.respond(html_resp(200, &html));
                                }
                                "direct-manga.777" => {
                                    let html = format!(
                                        r#"<html><body>
                                        <h1>Direct Hit Manga</h1>
                                        <div class="cover"><img src="{base}/img/cover_777.jpg" /></div>
                                        <div class="status">Completed</div>
                                        <div class="summary"><p>Single search result redirect test.</p></div>
                                        <div class="chapters">
                                            <a href="{base}/manga/direct-manga.777/c1">Chapter 1</a>
                                        </div>
                                        </body></html>"#
                                    );
                                    let _ = req.respond(html_resp(200, &html));
                                }
                                _ => {
                                    let _ = req.respond(
                                        Response::from_string("Manga Not Found")
                                            .with_status_code(404),
                                    );
                                }
                            }
                            continue;
                        }

                        if path.starts_with("/manga/") && path.contains("/c") {
                            chapter_hits.fetch_add(1, Ordering::SeqCst);

                            // Chapter 2 tests mirror fallback: only succeeds when ?sv=3 is passed
                            if path == "/manga/cyber-blade.101/c2" {
                                let sv = query.get("sv").map(|s| s.as_str()).unwrap_or("");
                                if sv != "3" {
                                    let _ = req.respond(html_resp(
                                        200,
                                        "<html><body>No thzq mirror here</body></html>",
                                    ));
                                    continue;
                                }
                                mirror_sv3_hits.fetch_add(1, Ordering::SeqCst);
                                let html = format!(
                                    r#"<html><script>var thzq = ['{base}/img/ch2_p1.jpg?token=1&amp;ok=2','{base}/img/ch2_p2_retry.png'];</script></html>"#
                                );
                                let _ = req.respond(html_resp(200, &html));
                                continue;
                            }

                            if path == "/manga/cyber-blade.101/c1" {
                                // 8 pages covering all supported formats + prefetch window > 5 (single-line for THZQ_RE)
                                let html = format!(
                                    r#"<html><script>var thzq = ['{base}/img/ch1_p1.jpg','{base}/img/ch1_p2.png','{base}/img/ch1_p3.webp','{base}/img/ch1_p4.gif','{base}/img/ch1_p5.avif','{base}/img/ch1_p6.jpg','{base}/img/ch1_p7.jpg','{base}/img/ch1_p8.jpg'];</script></html>"#
                                );
                                let _ = req.respond(html_resp(200, &html));
                                continue;
                            }

                            // Default 2-page chapter for c10.5, c11, padding-saga c1000, direct-manga c1
                            let slug = path.replace('/', "_");
                            let html = format!(
                                r#"<html><script>var thzq = ['{base}/img/{slug}_1.jpg','{base}/img/{slug}_2.png'];</script></html>"#
                            );
                            let _ = req.respond(html_resp(200, &html));
                            continue;
                        }

                        if let Some(img_name) = path.strip_prefix("/img/") {
                            image_hits.fetch_add(1, Ordering::SeqCst);

                            // Simulate a transient 502 error on first attempt for ch2_p2_retry.png
                            if img_name == "ch2_p2_retry.png" {
                                let attempt =
                                    transient_retry_hits.fetch_add(1, Ordering::SeqCst) + 1;
                                if attempt == 1 {
                                    let _ = req.respond(
                                        Response::from_string("Transient CDN Error")
                                            .with_status_code(502),
                                    );
                                    continue;
                                }
                            }

                            let (bytes, content_type) = build_synthetic_image(img_name);
                            let mut resp =
                                Response::from_data(bytes).with_status_code(StatusCode(200));
                            if let Some(ct) = content_type {
                                resp.add_header(
                                    Header::from_bytes(&b"Content-Type"[..], ct.as_bytes())
                                        .unwrap(),
                                );
                            }
                            let _ = req.respond(resp);
                            continue;
                        }

                        let _ = req.respond(Response::from_string("Not Found").with_status_code(404));
                    }
                })
                .expect("Failed to spawn mock upstream worker");
            workers.push(handle);
        }

        Ok(Self {
            port,
            base_url,
            search_hits,
            manga_hits,
            chapter_hits,
            mirror_sv3_hits,
            image_hits,
            transient_retry_hits,
            webhooks,
            enable_extra_chapter,
            server,
            workers,
        })
    }

    pub fn manga_hit_count(&self, id: &str) -> usize {
        self.manga_hits
            .lock()
            .unwrap()
            .get(id)
            .copied()
            .unwrap_or(0)
    }

    pub fn shutdown(self) {
        for _ in 0..self.workers.len() {
            self.server.unblock();
        }
        for w in self.workers {
            let _ = w.join();
        }
    }
}

fn html_resp(status: u16, body: &str) -> Response<std::io::Cursor<Vec<u8>>> {
    Response::from_string(body.to_string())
        .with_status_code(status)
        .with_header(
            Header::from_bytes(&b"Content-Type"[..], &b"text/html; charset=utf-8"[..]).unwrap(),
        )
}

/// Generates deterministic binary payloads with valid format magic bytes.
pub fn build_synthetic_image(name: &str) -> (Vec<u8>, Option<&'static str>) {
    let mut payload = Vec::with_capacity(4096);
    if name.ends_with(".png") {
        payload.extend_from_slice(b"\x89PNG\r\n\x1a\n");
        // Omit Content-Type header on retry image to verify magic-byte fallback detection
        let ct = if name.contains("retry") {
            None
        } else {
            Some("image/png")
        };
        payload.extend_from_slice(name.as_bytes());
        payload.resize(4096, 0x42);
        (payload, ct)
    } else if name.ends_with(".webp") {
        payload.extend_from_slice(b"RIFF\x00\x10\x00\x00WEBPVP8 ");
        payload.extend_from_slice(name.as_bytes());
        payload.resize(4096, 0x57);
        (payload, Some("image/webp"))
    } else if name.ends_with(".gif") {
        payload.extend_from_slice(b"GIF89a");
        payload.extend_from_slice(name.as_bytes());
        payload.resize(4096, 0x47);
        (payload, Some("image/gif"))
    } else if name.ends_with(".avif") {
        payload.extend_from_slice(b"\x00\x00\x00\x1cftypavif\x00\x00\x00\x00");
        payload.extend_from_slice(name.as_bytes());
        payload.resize(4096, 0x41);
        // Omit Content-Type to exercise AVIF magic-byte detection in `image_extension`
        (payload, None)
    } else {
        payload.extend_from_slice(b"\xff\xd8\xff\xe0\x00\x10JFIF\x00");
        payload.extend_from_slice(name.as_bytes());
        payload.resize(4096, 0xFF);
        (payload, Some("image/jpeg"))
    }
}
