//! The precip banner: each `#` in the banner text becomes a solid cell,
//! letters in off-white and the dotted i in green. The rendering follows
//! the way Omicron's wicket TUI draws its banners, and the palette is
//! wicket's (MPL-2.0, Oxide Computer Company). Oxide's own marks are not
//! used; see NOTICE.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Color;
use ratatui::widgets::Widget;

pub(crate) const PRECIP: &str = include_str!("../banners/precip.txt");

/// Palette from wicket's `ui/defaults/colors.rs`.
pub(crate) const OX_GREEN_LIGHT: Color = Color::Rgb(0x48, 0xD5, 0x97);
pub(crate) const OX_OFF_WHITE: Color = Color::Rgb(0xE0, 0xE0, 0xE0);
pub(crate) const OX_GRAY_DARK: Color = Color::Rgb(0x62, 0x66, 0x68);
pub(crate) const OX_YELLOW: Color = Color::Rgb(0xF5, 0xCF, 0x65);

pub(crate) const NAME: &str = "precip";
pub(crate) const TAGLINE: &str = "AWS estate → Oxide rack sizing";

/// Width in cells of the widest banner line.
pub(crate) fn width(text: &str) -> u16 {
    text.lines()
        .map(|l| l.chars().count())
        .max()
        .and_then(|w| u16::try_from(w).ok())
        .unwrap_or(0)
}

pub(crate) fn height(text: &str) -> u16 {
    u16::try_from(text.lines().count()).unwrap_or(0)
}

pub(crate) struct Banner {
    text: &'static str,
    /// Columns painted in the accent colour: the i in "precip".
    accent_columns: std::ops::RangeInclusive<usize>,
}

impl Banner {
    pub(crate) fn precip() -> Self {
        Self { text: PRECIP, accent_columns: 28..=33 }
    }
}

impl Widget for Banner {
    fn render(self, area: Rect, buf: &mut Buffer) {
        for (y, line) in self.text.lines().enumerate() {
            let Ok(y) = u16::try_from(y) else { break };
            if y >= area.height {
                break;
            }
            for (x, c) in line.chars().enumerate() {
                let Ok(x) = u16::try_from(x) else { break };
                if x >= area.width || c != '#' {
                    continue;
                }
                let color = if self.accent_columns.contains(&usize::from(x)) {
                    OX_GREEN_LIGHT
                } else {
                    OX_OFF_WHITE
                };
                buf[(area.left() + x, area.top() + y)]
                    .set_symbol(" ")
                    .set_bg(color);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn banner_is_six_glyphs_of_six_columns() {
        assert_eq!((width(PRECIP), height(PRECIP)), (41, 7));
    }

    #[test]
    fn accent_columns_cover_only_the_i() {
        // Row 2: columns 28..=33 are the i, with a gap either side.
        let row: Vec<char> = PRECIP.lines().nth(2).unwrap().chars().collect();
        assert!(row[28..=33].contains(&'#'));
        assert_eq!(row[27], ' ');
        assert_eq!(row[34], ' ');
        // Row 0 is only the dot of the i.
        let top: Vec<char> = PRECIP.lines().next().unwrap().chars().collect();
        assert!(top[..28].iter().all(|c| *c == ' '));
        assert!(top[28..=33].contains(&'#'));
    }
}
