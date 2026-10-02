use iced::{Length, Theme, widget::svg};

/// Application-owned 24 px line icons. Every asset shares the same stroke,
/// caps, joins, and view box so rendering does not depend on platform glyphs.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LineIcon {
    Brand,
    Add,
    Refresh,
    Settings,
    Fullscreen,
    Terminal,
    Collapse,
    Delete,
    Close,
    Ready,
    Working,
    NeedsFeedback,
    Finished,
    Failed,
    Stopped,
    Disconnected,
}

impl LineIcon {
    const fn source(self) -> &'static [u8] {
        match self {
            Self::Brand => br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M5 21c5-1 9-5 11-11"/><path d="M7 17C3 12 6 5 19 3c1 11-5 15-12 14Z"/></svg>"#,
            Self::Add => br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M12 5v14M5 12h14"/></svg>"#,
            Self::Refresh => br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M20 6v5h-5"/><path d="M4 18v-5h5"/><path d="M6 9a7 7 0 0 1 11-2l3 4M4 13l3 4a7 7 0 0 0 11-2"/></svg>"#,
            Self::Settings => br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><circle cx="12" cy="12" r="3"/><path d="M19 13.5v-3l-2-.7-.7-1.7.9-1.9-2.1-2.1-1.9.9-1.7-.7L10.5 2h-3l-.7 2-1.7.7-1.9-.9-2.1 2.1.9 1.9-.7 1.7-2 .7v3l2 .7.7 1.7-.9 1.9 2.1 2.1 1.9-.9 1.7.7.7 2h3l.7-2 1.7-.7 1.9.9 2.1-2.1-.9-1.9.7-1.7Z" transform="translate(1.5) scale(.88)"/></svg>"#,
            Self::Fullscreen => br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M8 3H3v5m13-5h5v5M8 21H3v-5m13 5h5v-5"/></svg>"#,
            Self::Terminal => br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><rect x="3" y="4" width="18" height="16" rx="2"/><path d="m7 9 3 3-3 3m6 0h4"/></svg>"#,
            Self::Collapse => br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="m9 18-6-6 6-6m6 0 6 6-6 6"/></svg>"#,
            Self::Delete => br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M4 7h16m-10 4v6m4-6v6M9 7l1-3h4l1 3m3 0-1 14H7L6 7"/></svg>"#,
            Self::Close => br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="m7 7 10 10m0-10L7 17"/></svg>"#,
            Self::Ready => br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><circle cx="12" cy="12" r="8"/></svg>"#,
            Self::Working => br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><circle cx="12" cy="12" r="9"/><path d="m10 8 6 4-6 4Z"/></svg>"#,
            Self::NeedsFeedback => br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M4 5h16v11H9l-5 4Z"/><path d="M12 8v4m0 2h.01"/></svg>"#,
            Self::Finished => br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><circle cx="12" cy="12" r="9"/><path d="m8 12 3 3 5-6"/></svg>"#,
            Self::Failed => br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><circle cx="12" cy="12" r="9"/><path d="m9 9 6 6m0-6-6 6"/></svg>"#,
            Self::Stopped => br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><rect x="5" y="5" width="14" height="14" rx="2"/></svg>"#,
            Self::Disconnected => br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M5 5 19 19"/><path d="M8 4v4m8 8v4M4 8h4m8 8h4"/></svg>"#,
        }
    }
}

pub(crate) fn line_icon(icon: LineIcon, size: u16) -> svg::Svg<'static> {
    svg::Svg::new(svg::Handle::from_memory(icon.source()))
        .width(Length::Fixed(f32::from(size)))
        .height(Length::Fixed(f32::from(size)))
        .style(|theme: &Theme, _status| svg::Style {
            color: Some(theme.palette().text),
        })
}

#[cfg(test)]
mod tests {
    use super::LineIcon;

    #[test]
    fn every_icon_uses_the_same_bounded_stroke_contract() {
        for icon in [
            LineIcon::Brand,
            LineIcon::Add,
            LineIcon::Refresh,
            LineIcon::Settings,
            LineIcon::Fullscreen,
            LineIcon::Terminal,
            LineIcon::Collapse,
            LineIcon::Delete,
            LineIcon::Close,
            LineIcon::Ready,
            LineIcon::Working,
            LineIcon::NeedsFeedback,
            LineIcon::Finished,
            LineIcon::Failed,
            LineIcon::Stopped,
            LineIcon::Disconnected,
        ] {
            let source = std::str::from_utf8(icon.source()).unwrap();
            assert!(source.contains("viewBox=\"0 0 24 24\""));
            assert!(source.contains("fill=\"none\""));
            assert!(source.contains("stroke=\"currentColor\""));
            assert!(source.contains("stroke-width=\"2\""));
            assert!(source.contains("stroke-linecap=\"round\""));
            assert!(source.contains("stroke-linejoin=\"round\""));
            assert!(source.len() < 512);
        }
    }
}
