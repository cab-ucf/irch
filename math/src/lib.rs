//! Typst math as MathML: no code, files or packages, so it only typesets.

use std::sync::LazyLock;
use typst::diag::{FileError, FileResult};
use typst::foundations::{Bytes, Datetime, Duration};
use typst::syntax::{FileId, RootedPath, Source, VirtualPath, VirtualRoot};
use typst::text::{Font, FontBook};
use typst::utils::LazyHash;
use typst::{Feature, Library, LibraryExt, World};
use typst_html::HtmlDocument;

const MAX: usize = 2048;

static LIB: LazyLock<LazyHash<Library>> = LazyLock::new(|| {
    let html = [Feature::Html].into_iter().collect();
    LazyHash::new(Library::builder().with_features(html).build())
});
static BOOK: LazyLock<LazyHash<FontBook>> =
    LazyLock::new(|| LazyHash::new(FontBook::new()));

struct Math(Source);

impl World for Math {
    fn library(&self) -> &LazyHash<Library> {
        &LIB
    }
    fn book(&self) -> &LazyHash<FontBook> {
        &BOOK
    }
    fn main(&self) -> FileId {
        self.0.id()
    }
    fn source(&self, id: FileId) -> FileResult<Source> {
        (id == self.0.id())
            .then(|| self.0.clone())
            .ok_or(FileError::AccessDenied)
    }
    fn file(&self, _: FileId) -> FileResult<Bytes> {
        Err(FileError::AccessDenied)
    }
    fn font(&self, _: usize) -> Option<Font> {
        None
    }
    fn today(&self, _: Option<Duration>) -> Option<Datetime> {
        None
    }
}

pub fn mathml(src: &str, display: bool) -> Result<String, String> {
    // `#` and `$` leave math, `std` reaches all of Typst (endless loops,
    // memory-filling arrays), and a string's methods compute.
    let code = src.contains(['#', '$']) || src.contains("std");
    if src.len() > MAX || code || src.contains("\".") {
        return Err("not plain Typst math".into());
    }
    let pad = if display { " " } else { "" };
    let html = html(format!("${pad}{src}{pad}$"))?;
    let (a, b) = (html.find("<math"), html.rfind("</math>"));
    match (a, b) {
        (Some(a), Some(b)) => Ok(html[a..b + 7].into()),
        _ => Err("no math".into()),
    }
}

fn html(text: String) -> Result<String, String> {
    let path = VirtualPath::new("m.typ").map_err(|e| format!("{e:?}"))?;
    let id = FileId::new(RootedPath::new(VirtualRoot::Project, path));
    let doc = typst::compile::<HtmlDocument>(&Math(Source::new(id, text)));
    typst::comemo::evict(10);
    let err = |e: typst::ecow::EcoVec<typst::diag::SourceDiagnostic>| {
        e.first().map_or("typst failed".into(), |d| d.message.to_string())
    };
    typst_html::html(&doc.output.map_err(err)?, &Default::default())
        .map_err(err)
}

#[cfg(test)]
mod tests {
    use super::{html, mathml};

    #[test]
    fn typesets_math_and_nothing_else() {
        let m = mathml("x^2", false).unwrap();
        assert!(m.starts_with("<math><msup>") && m.ends_with("</math>"));
        let d = mathml("sum_(k=1)^n k = (n(n+1))/2", true).unwrap();
        assert!(d.starts_with(r#"<math display="block">"#));
        assert!(d.contains("<mfrac>") && d.contains("<munderover>"));
        assert!(mathml(r#"#read("/etc/passwd")"#, false).is_err());
        assert!(mathml("x$ #panic()", false).is_err());
        assert!(mathml("frac(", false).is_err());
        assert!(
            mathml(r#"std.range(std.int("100000000000"))"#, false).is_err()
        );
        assert!(mathml(r#""a".repeat(99999999)"#, false).is_err());
    }

    #[test]
    fn the_vendored_stylesheet_is_typsts() {
        let h = html("$x$".into()).unwrap();
        let css =
            &h[h.find("<style>").unwrap() + 7..h.find("</style>").unwrap()];
        let ours = include_str!("../../app/ui/vendor/typst-math.css");
        assert_eq!(ours.trim_end(), css.trim_end());
    }
}
