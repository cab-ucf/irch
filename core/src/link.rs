//! Link previews, made by a message's sender as it is sent: only the
//! sender's device asks the site, and the preview travels sealed in the
//! space. Pages are read as text for their meta tags, never run.

use crate::op::{Link, clip};
use anyhow::{Result, bail, ensure};
use std::collections::HashMap;

// YouTube's tags come after 750 KB of scripts; reading stops at </head>.
const PAGE: usize = 2 << 20;
const IMAGE: usize = 2 << 20;

/// The first https link in a message.
pub fn first(text: &str) -> Option<&str> {
    let edge = |c: char| "<>()[]{}\"'`,.;:!?*_~|".contains(c);
    let words = text.split_whitespace().map(|w| w.trim_matches(edge));
    words.into_iter().find(|w| w.starts_with("https://") && w.len() > 8)
}

/// Its title, description and image (bytes and extension), if any.
pub async fn fetch(url: &str) -> Result<(Link, Option<(Vec<u8>, &str)>)> {
    let web = crate::node::web();
    let (page, _) = get(&web, url, PAGE, "text/html").await?;
    let html = String::from_utf8_lossy(&page);
    let meta = metas(&html);
    let pick = |ks: &[&str]| ks.iter().find_map(|k| meta.get(*k)).cloned();
    let title = pick(&["og:title", "twitter:title"]).or_else(|| title(&html));
    let text = pick(&["og:description", "twitter:description", "description"]);
    let (title, text) = (title.unwrap_or_default(), text.unwrap_or_default());
    ensure!(!title.is_empty() || !text.is_empty(), "nothing to preview");
    let image = match pick(&["og:image", "twitter:image"]) {
        Some(src) => {
            let at = reqwest::Url::parse(url)?.join(&unescape(&src))?;
            get(&web, at.as_str(), IMAGE, "image/").await.ok()
        }
        None => None,
    };
    let image = image.and_then(|(b, kind)| {
        let ext = match kind.as_str() {
            "image/png" => "png",
            "image/jpeg" => "jpg",
            "image/webp" => "webp",
            "image/gif" => "gif",
            "image/avif" => "avif",
            _ => return None,
        };
        Some((b, ext))
    });
    let link = Link {
        url: url.into(),
        title: clip(&unescape(&title), 200),
        text: clip(&unescape(&text), 400),
        image: None,
    };
    Ok((link, image))
}

/// At most `max` bytes of a `kind` body, and its type.
async fn get(
    web: &reqwest::Client,
    url: &str,
    max: usize,
    kind: &str,
) -> Result<(Vec<u8>, String)> {
    let mut r = web.get(url).send().await?.error_for_status()?;
    let ct = r.headers().get(reqwest::header::CONTENT_TYPE);
    let ct = ct.and_then(|v| v.to_str().ok()).unwrap_or_default();
    let ct = ct.split(';').next().unwrap_or_default().trim().to_lowercase();
    if !ct.starts_with(kind) {
        bail!("not {kind}: {ct}");
    }
    let mut b = vec![];
    let html = kind == "text/html";
    while let Some(c) = r.chunk().await? {
        b.extend_from_slice(&c);
        let tail = &b[b.len().saturating_sub(c.len() + 6)..];
        if html && tail.windows(7).any(|w| w.eq_ignore_ascii_case(b"</head>")) {
            break;
        }
        if b.len() > max {
            // A page's head is enough; an image cut short is none.
            ensure!(html, "too big");
            b.truncate(max);
            break;
        }
    }
    Ok((b, ct))
}

/// `<meta property|name=K content=V>`, keys lowercased.
fn metas(html: &str) -> HashMap<String, String> {
    let low = html.to_ascii_lowercase();
    let mut out = HashMap::new();
    for (at, _) in low.match_indices("<meta") {
        let end = low[at..].find('>').map_or(low.len(), |e| at + e);
        let a = attrs(&html[at + 5..end]);
        let key = a.get("property").or_else(|| a.get("name"));
        if let (Some(k), Some(v)) = (key, a.get("content")) {
            out.entry(k.to_lowercase()).or_insert_with(|| v.clone());
        }
    }
    out
}

/// A tag's `k="v"`, `k='v'` and `k=v`, keys lowercased.
fn attrs(s: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let mut rest = s;
    while let Some(eq) = rest.find('=') {
        let key = rest[..eq].split_whitespace().last().unwrap_or_default();
        let v = rest[eq + 1..].trim_start();
        let (val, next) = match v.chars().next() {
            Some(q @ ('"' | '\'')) => match v[1..].find(q) {
                Some(e) => (&v[1..e + 1], &v[e + 2..]),
                None => (&v[1..], ""),
            },
            _ => {
                let e = v.find(char::is_whitespace).unwrap_or(v.len());
                (&v[..e], &v[e..])
            }
        };
        out.insert(key.to_lowercase(), val.to_string());
        rest = next;
    }
    out
}

fn title(html: &str) -> Option<String> {
    let low = html.to_ascii_lowercase();
    let open = low.find("<title")?;
    let start = open + low[open..].find('>')? + 1;
    let end = start + low[start..].find("</title")?;
    Some(html[start..end].to_string())
}

fn unescape(s: &str) -> String {
    let pairs = [
        ("&quot;", "\""),
        ("&#39;", "'"),
        ("&#x27;", "'"),
        ("&apos;", "'"),
        ("&lt;", "<"),
        ("&gt;", ">"),
        ("&nbsp;", " "),
        ("&amp;", "&"),
    ];
    pairs.iter().fold(s.to_string(), |s, (a, b)| s.replace(a, b))
}

#[test]
fn reads_what_a_page_says_of_itself() {
    let html = r#"<html><head><TITLE>Plain</TITLE>
        <meta property="og:title" content="Tom &amp; Jerry">
        <META name=description content='A "cat" and a mouse'>
        <meta property="og:image" content="/i.png"></head>"#;
    let m = metas(html);
    assert_eq!(unescape(&m["og:title"]), "Tom & Jerry");
    assert_eq!(m["description"], r#"A "cat" and a mouse"#);
    assert_eq!(m["og:image"], "/i.png");
    assert_eq!(title(html).as_deref(), Some("Plain"));
    assert_eq!(
        first("see (https://ex.com/a?b=1)."),
        Some("https://ex.com/a?b=1")
    );
    assert_eq!(first("http://ex.com https:// x"), None);
}
