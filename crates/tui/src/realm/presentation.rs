//! Client-local presentation policy. Domain actions, state, and themes stay shared.
use tuirealm::props::{AttrValue, Attribute};
use tuirealm::ratatui::layout::Rect;

/// Selectable per launch, so phone and laptop clients can share a daemon.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum Presentation {
    #[default]
    Desktop,
    Mobile,
}

pub(crate) const MOBILE_ATTRIBUTE: Attribute = Attribute::Custom("mobile");

impl Presentation {
    pub(crate) fn apply_attribute(&mut self, attr: Attribute, value: AttrValue) {
        if let (MOBILE_ATTRIBUTE, AttrValue::Flag(mobile)) = (attr, value) {
            *self = if mobile { Self::Mobile } else { Self::Desktop };
        }
    }

    /// Mobile sheets use the available width, with no desktop side gutters.
    pub(crate) fn modal(self, area: Rect, width: u16, height: u16) -> Rect {
        let (width, height) = if self == Self::Mobile {
            (area.width, height.min(area.height))
        } else {
            (
                width.min(area.width.saturating_sub(4)),
                height.min(area.height.saturating_sub(4)),
            )
        };
        Rect::new(
            area.x + area.width.saturating_sub(width) / 2,
            area.y + area.height.saturating_sub(height) / 2,
            width,
            height,
        )
    }
}

/// Cell-width aware wrapping shared by compact sheets, including long repo names.
pub(crate) fn wrap_text(text: &str, width: u16) -> Vec<String> {
    let width = usize::from(width.max(1));
    let mut lines = Vec::new();
    for paragraph in text.split('\n') {
        let mut line = String::new();
        for word in paragraph.split_whitespace() {
            if !line.is_empty()
                && crate::util::visual_width(&line) + 1 + crate::util::visual_width(word) > width
            {
                lines.push(std::mem::take(&mut line));
            }
            if !line.is_empty() {
                line.push(' ');
            }
            for ch in crate::util::graphemes(word) {
                if !line.is_empty()
                    && crate::util::visual_width(&line) + crate::util::visual_width(ch) > width
                {
                    lines.push(std::mem::take(&mut line));
                }
                line.push_str(ch);
            }
        }
        lines.push(line);
    }
    lines
}

pub(crate) fn render_welcome(frame: &mut tuirealm::ratatui::Frame, area: Rect) {
    use tuirealm::ratatui::{
        style::Style,
        widgets::{Block, BorderType, Borders, Clear, Paragraph, Wrap},
    };
    let theme = crate::theme::current();
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .title(" lazybox mobile ")
        .border_style(theme.modal_border());
    let inner = block.inner(area);
    frame.render_widget(Clear, area);
    frame.render_widget(block, area);
    if inner.height == 0 {
        return;
    }
    let body = Rect::new(
        inner.x,
        inner.y,
        inner.width,
        inner.height.saturating_sub(1),
    );
    frame.render_widget(Paragraph::new("Your sessions, one pane at a time.\n\nChoose task sources and agents next.\n\nj/k move · Space select\nEnter continue · Esc cancel\n\nCtrl-T opens Sessions.\nn new · r rename · x delete.")
        .wrap(Wrap { trim: false }).style(Style::default().fg(theme.text_strong)), body);
    frame.render_widget(
        Paragraph::new("Enter setup   Esc quit").style(Style::default().fg(theme.accent)),
        Rect::new(inner.x, inner.bottom().saturating_sub(1), inner.width, 1),
    );
}

/// A full-width, scrolling information sheet with a pinned action row.
pub(crate) fn render_reader(
    frame: &mut tuirealm::ratatui::Frame,
    area: Rect,
    title: &str,
    body: &str,
    scroll: &mut u16,
    hint: &str,
) {
    use tuirealm::ratatui::{
        style::Style,
        text::Line,
        widgets::{Block, BorderType, Borders, Clear, Paragraph},
    };
    let theme = crate::theme::current();
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .title(format!(" {title} "))
        .border_style(theme.modal_border());
    let inner = block.inner(area);
    frame.render_widget(Clear, area);
    frame.render_widget(block, area);
    if inner.height == 0 {
        return;
    }
    let body_area = Rect::new(
        inner.x,
        inner.y,
        inner.width,
        inner.height.saturating_sub(1),
    );
    let lines: Vec<_> = wrap_text(body, inner.width)
        .into_iter()
        .map(Line::raw)
        .collect();
    let max_scroll = lines.len().saturating_sub(usize::from(body_area.height));
    *scroll = (*scroll).min(max_scroll.min(u16::MAX as usize) as u16);
    frame.render_widget(
        Paragraph::new(lines)
            .scroll((*scroll, 0))
            .style(Style::default().fg(theme.text_strong)),
        body_area,
    );
    frame.render_widget(
        Paragraph::new(hint).style(Style::default().fg(theme.accent)),
        Rect::new(inner.x, inner.bottom() - 1, inner.width, 1),
    );
}

pub(crate) fn mobile_header(area: Rect) -> (Rect, Rect) {
    let height = area.height.min(1);
    (
        Rect::new(area.x, area.y, area.width, height),
        Rect::new(area.x, area.y + height, area.width, area.height - height),
    )
}

/// Keep the terminal geometry identical with the rail open or closed.
pub(crate) fn mobile_terminal(area: Rect) -> (Rect, Rect) {
    let width = area.width.min(1);
    (
        Rect::new(area.x, area.y, width, area.height),
        Rect::new(area.x + width, area.y, area.width - width, area.height),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sheets_stay_inside_tiny_and_phone_viewports() {
        for (w, h) in [(0, 0), (1, 1), (20, 5), (39, 12), (60, 24), (120, 40)] {
            let area = Rect::new(2, 3, w, h);
            let sheet = Presentation::Mobile.modal(area, 80, 24);
            assert_eq!(sheet.width, w);
            assert_eq!(sheet.intersection(area), sheet);
        }
    }
}
