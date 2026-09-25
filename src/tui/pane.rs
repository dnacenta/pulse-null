//! Panes: the Hyprland window model one level down. Each region is a rounded
//! pane with a one-cell gap; exactly one is focused and carries the accent
//! border; focus moves spatially with Ctrl+h/j/k/l.

use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::widgets::{Block, BorderType};

use super::theme::Tokens;

/// Every pane the shell knows about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum PaneId {
    Transcript,
    Prompt,
}

/// A direction of focus travel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dir {
    Left,
    Down,
    Up,
    Right,
}

/// One cell of air on every side, the way Hyprland gaps windows.
#[must_use]
pub fn gap(rect: Rect) -> Rect {
    Rect {
        x: rect.x.saturating_add(1),
        y: rect.y.saturating_add(1),
        width: rect.width.saturating_sub(2),
        height: rect.height.saturating_sub(2),
    }
}

/// The frame every pane draws: rounded, accent when focused, dim otherwise.
#[must_use]
pub fn frame(title: &str, focused: bool, t: Tokens) -> Block<'static> {
    let border = if focused { t.accent } else { t.dim };
    let title_style = if focused {
        Style::default().fg(t.accent)
    } else {
        Style::default().fg(t.dim)
    };
    let mut b = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(border))
        .style(Style::default().bg(t.ground));
    if !title.is_empty() {
        b = b.title(Line::from(format!(" {title} ")).style(title_style));
    }
    b
}

/// Pick the pane in direction `dir` from `from`, by edge distance, among
/// `panes` = (id, rect). Returns `from` when nothing lies that way.
#[must_use]
pub fn neighbour(from: PaneId, dir: Dir, panes: &[(PaneId, Rect)]) -> PaneId {
    let Some(&(_, origin)) = panes.iter().find(|(id, _)| *id == from) else {
        return from;
    };
    let (ox, oy) = center(origin);
    let mut best: Option<(i64, PaneId)> = None;
    for &(id, r) in panes {
        if id == from {
            continue;
        }
        let (cx, cy) = center(r);
        let ahead = match dir {
            Dir::Left => cx < ox,
            Dir::Right => cx > ox,
            Dir::Up => cy < oy,
            Dir::Down => cy > oy,
        };
        if !ahead {
            continue;
        }
        // Manhattan distance, weighting the off-axis so a pane directly
        // ahead wins over a diagonal one.
        let (dx, dy) = ((cx - ox).abs(), (cy - oy).abs());
        let score = match dir {
            Dir::Left | Dir::Right => dx + dy * 3,
            Dir::Up | Dir::Down => dy + dx * 3,
        };
        if best.is_none_or(|(s, _)| score < s) {
            best = Some((score, id));
        }
    }
    best.map_or(from, |(_, id)| id)
}

fn center(r: Rect) -> (i64, i64) {
    (
        i64::from(r.x) + i64::from(r.width) / 2,
        i64::from(r.y) + i64::from(r.height) / 2,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn neighbour_moves_down_and_stops_at_edges() {
        let panes = [
            (PaneId::Transcript, Rect::new(0, 0, 80, 20)),
            (PaneId::Prompt, Rect::new(0, 20, 80, 3)),
        ];
        assert_eq!(
            neighbour(PaneId::Transcript, Dir::Down, &panes),
            PaneId::Prompt
        );
        assert_eq!(
            neighbour(PaneId::Prompt, Dir::Up, &panes),
            PaneId::Transcript
        );
        assert_eq!(neighbour(PaneId::Prompt, Dir::Down, &panes), PaneId::Prompt);
        assert_eq!(
            neighbour(PaneId::Transcript, Dir::Left, &panes),
            PaneId::Transcript
        );
    }

    #[test]
    fn gap_shrinks_by_one_cell_each_side() {
        let g = gap(Rect::new(2, 3, 10, 5));
        assert_eq!(g, Rect::new(3, 4, 8, 3));
        assert_eq!(gap(Rect::new(0, 0, 1, 1)).width, 0);
    }
}
