//! Web/markup extractor plugins: HTML, XML, SVG.

mod html;
mod svg;
mod xml;

pub use html::HtmlExtractor;
pub use svg::SvgExtractor;
pub use xml::XmlExtractor;
