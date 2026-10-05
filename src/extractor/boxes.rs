//! Printed form boxes: keep the text drawn inside one box together.
//!
//! Tax and bank forms draw each field as a ruled box, a small label at the
//! top and the value below it. A rule elsewhere in the row (a neighbouring
//! box split into two entries) cuts page-wide table grids and line grouping
//! straight through such a box, and the value is read lines away from its
//! label, after the next boxes' text. Merging the text of each small closed
//! box into one item, label first, keeps the two together whatever layout
//! the page later gets.

use std::collections::HashMap;

use crate::types::{ItemType, PdfLine, PdfRect, TextItem, TextLine};

/// Form boxes hold a label and a value or two; anything taller is a
/// bordered block of prose or a table, and keeps its lines.
const MAX_BOX_HEIGHT: f32 = 60.0;
const MAX_BOX_LINES: usize = 4;
/// How far a rule's end may fall short of the point it should cover.
const RULE_SLACK: f32 = 1.5;
/// A filled rect this thin is a drawn rule. Wider rects are shading
/// (an entry area's tint, a highlighted band) or checkbox squares, whose
/// edges bound no field.
const MAX_RULE_RECT_THICKNESS: f32 = 2.0;

/// Merge the printed text inside each small drawn box on one page into a
/// single item, read line by line from the top.
///
/// Text shares a box when the same rules bound it above, below and to the
/// left: no rule separates its lines. Only boxes closed on all four sides,
/// at most [`MAX_BOX_HEIGHT`] tall, holding two to [`MAX_BOX_LINES`] lines
/// and with a line set in a different face from the first (a filled value
/// under its printed label) are merged. Form-field values keep their own
/// placement, and a page without drawn rules is left as it is.
pub(crate) fn merge_boxed_text(items: &mut Vec<TextItem>, rects: &[PdfRect], lines: &[PdfLine]) {
    let rules = Rules::new(rects, lines);
    if rules.horizontal.is_empty() || rules.vertical.is_empty() {
        return;
    }
    let mut boxes: HashMap<(i32, i32, i32), Vec<usize>> = HashMap::new();
    for (index, item) in items.iter().enumerate() {
        if !matches!(item.item_type, ItemType::Text) || item.text.trim().is_empty() {
            continue;
        }
        if let Some(walls) = rules.walls(item) {
            boxes.entry(walls).or_default().push(index);
        }
    }

    let mut merged: Vec<(usize, TextItem)> = Vec::new();
    let mut remove = vec![false; items.len()];
    for ((top, bottom, _), members) in boxes {
        if members.len() < 2 || (top - bottom) as f32 / 2.0 > MAX_BOX_HEIGHT {
            continue;
        }
        let Some(item) = merge_box(items, &members) else {
            continue;
        };
        let first = *members.iter().min().expect("box has members");
        for &member in &members {
            remove[member] = true;
        }
        log::trace!(
            "merged box: {:?}",
            super::trace_text_preview(&item.text, 80)
        );
        merged.push((first, item));
    }
    if let Some(item) = items.first() {
        log::debug!("page {}: {} form boxes merged", item.page, merged.len());
    }
    for (first, item) in merged {
        items[first] = item;
        remove[first] = false;
    }
    let mut index = 0;
    items.retain(|_| {
        index += 1;
        !remove[index - 1]
    });
}

/// One item holding a box's text, line by line from the top, at its first
/// line's baseline. `None` when the text sits on a single line (nothing to
/// keep together) or on too many to be a form box.
fn merge_box(items: &[TextItem], members: &[usize]) -> Option<TextItem> {
    let mut parts: Vec<&TextItem> = members.iter().map(|&index| &items[index]).collect();
    parts.sort_by(|a, b| b.y.total_cmp(&a.y).then(a.x.total_cmp(&b.x)));
    // Items share a line when their glyphs overlap vertically, so a raised
    // footnote marker or a subscript stays on the line it sits on.
    let glyphs = |item: &TextItem| {
        let size = item.font_size.max(1.0);
        (item.y - size * 0.25, item.y + size * 0.75)
    };
    let mut box_lines: Vec<Vec<TextItem>> = Vec::new();
    for part in parts {
        let (low, high) = glyphs(part);
        let same_line = box_lines.last().is_some_and(|line| {
            line.iter().any(|other| {
                let (other_low, other_high) = glyphs(other);
                let overlap = high.min(other_high) - low.max(other_low);
                overlap >= 0.5 * (high - low).min(other_high - other_low)
            })
        });
        if same_line {
            box_lines
                .last_mut()
                .expect("line exists")
                .push(part.clone());
        } else {
            box_lines.push(vec![part.clone()]);
        }
    }
    if box_lines.len() < 2 || box_lines.len() > MAX_BOX_LINES {
        return None;
    }
    // A form box's value is filled in a face or size of its own below the
    // printed label; lines all in one face are a table cell's wrapped text,
    // which table detection reads line by line.
    let face = |line: &[TextItem]| {
        let main = main_run(line);
        (main.font.clone(), (main.font_size * 2.0).round() as i32)
    };
    if box_lines
        .iter()
        .all(|line| face(line) == face(&box_lines[0]))
    {
        return None;
    }
    // The merged item reads at the label's line, in the label's face.
    let base = main_run(&box_lines[0]).clone();
    let all = |flag: fn(&TextItem) -> bool| box_lines.iter().flatten().all(flag);
    let left = box_lines
        .iter()
        .flatten()
        .map(|i| i.x)
        .fold(f32::INFINITY, f32::min);
    let right = box_lines
        .iter()
        .flatten()
        .map(|i| i.x + i.width)
        .fold(f32::NEG_INFINITY, f32::max);
    let (is_bold, is_italic) = (all(|i| i.is_bold), all(|i| i.is_italic));
    let (is_underline, is_strikeout) = (all(|i| i.is_underline), all(|i| i.is_strikeout));
    let text = box_lines
        .into_iter()
        .map(|mut line_items| {
            crate::text_utils::sort_line_items(&mut line_items);
            let first = &line_items[0];
            let (y, page) = (first.y, first.page);
            TextLine {
                items: line_items,
                y,
                page,
                adaptive_threshold: 0.10,
            }
            .text()
            .trim()
            .to_string()
        })
        .collect::<Vec<_>>()
        .join(" ");
    Some(TextItem {
        text,
        x: left,
        width: right - left,
        is_bold,
        is_italic,
        is_underline,
        is_strikeout,
        ..base
    })
}

/// A line's main run: its longest at full size, so a subscript or
/// superscript never stands for the line.
fn main_run(line: &[TextItem]) -> &TextItem {
    let full = line.iter().map(|item| item.font_size).fold(0.0, f32::max) * 0.8;
    line.iter()
        .filter(|item| item.font_size >= full)
        .max_by_key(|item| item.text.trim().chars().count())
        .expect("a line has a full-size run")
}

/// A page's axis-aligned rules: stroked lines and rects thin enough to be
/// lines.
struct Rules {
    /// `(y, x_start, x_end)`
    horizontal: Vec<(f32, f32, f32)>,
    /// `(x, y_start, y_end)`
    vertical: Vec<(f32, f32, f32)>,
}

impl Rules {
    fn new(rects: &[PdfRect], lines: &[PdfLine]) -> Self {
        let mut horizontal = Vec::new();
        let mut vertical = Vec::new();
        for line in lines {
            let (x0, x1) = (line.x1.min(line.x2), line.x1.max(line.x2));
            let (y0, y1) = (line.y1.min(line.y2), line.y1.max(line.y2));
            if y1 - y0 <= 1.0 && x1 - x0 > 1.0 {
                horizontal.push(((y0 + y1) / 2.0, x0, x1));
            } else if x1 - x0 <= 1.0 && y1 - y0 > 1.0 {
                vertical.push(((x0 + x1) / 2.0, y0, y1));
            }
        }
        for rect in rects {
            let (x0, x1) = (
                rect.x.min(rect.x + rect.width),
                rect.x.max(rect.x + rect.width),
            );
            let (y0, y1) = (
                rect.y.min(rect.y + rect.height),
                rect.y.max(rect.y + rect.height),
            );
            if y1 - y0 <= MAX_RULE_RECT_THICKNESS && x1 - x0 > 1.0 {
                horizontal.push(((y0 + y1) / 2.0, x0, x1));
            } else if x1 - x0 <= MAX_RULE_RECT_THICKNESS && y1 - y0 > 1.0 {
                vertical.push(((x0 + x1) / 2.0, y0, y1));
            }
        }
        Self {
            horizontal,
            vertical,
        }
    }

    /// The rules closing in an item: the nearest one above and below its
    /// glyphs and to its left and right, as `(top, bottom, left)` in
    /// half-points. The right wall must exist but is not part of the box's
    /// identity: a sub-box ruled into one corner of a field box stands
    /// between the label and the box's own right edge, but not beside the
    /// value.
    fn walls(&self, item: &TextItem) -> Option<(i32, i32, i32)> {
        let size = item.font_size.max(1.0);
        let (glyph_top, glyph_bottom) = (item.y + size * 0.7, item.y - size * 0.25);
        let (mid_x, mid_y) = (item.x + item.width / 2.0, item.y + size * 0.3);
        let spans_x =
            |&&(_, x0, x1): &&(f32, f32, f32)| x0 - RULE_SLACK <= mid_x && mid_x <= x1 + RULE_SLACK;
        let spans_y =
            |&&(_, y0, y1): &&(f32, f32, f32)| y0 - RULE_SLACK <= mid_y && mid_y <= y1 + RULE_SLACK;
        let rows = || self.horizontal.iter().filter(spans_x).map(|&(y, _, _)| y);
        let top = rows().filter(|&y| y >= glyph_top - 0.5).reduce(f32::min)?;
        let bottom = rows()
            .filter(|&y| y <= glyph_bottom + 0.5)
            .reduce(f32::max)?;
        let left = self
            .vertical
            .iter()
            .filter(spans_y)
            .map(|&(x, _, _)| x)
            .filter(|&x| x <= item.x + 1.0)
            .reduce(f32::max)?;
        let right_edge = item.x + item.width;
        self.vertical
            .iter()
            .filter(spans_y)
            .any(|&(x, _, _)| x >= right_edge - 1.0)
            .then_some(())?;
        let half_points = |v: f32| (v * 2.0).round() as i32;
        Some((half_points(top), half_points(bottom), half_points(left)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ItemType;

    fn text(text: &str, x: f32, y: f32, width: f32, font_size: f32) -> TextItem {
        TextItem {
            text: text.to_string(),
            x,
            y,
            width,
            height: font_size,
            font: "F1".to_string(),
            font_size,
            page: 1,
            is_bold: false,
            is_italic: false,
            is_underline: false,
            is_strikeout: false,
            item_type: ItemType::Text,
            mcid: None,
        }
    }

    fn line(x1: f32, y1: f32, x2: f32, y2: f32) -> PdfLine {
        PdfLine {
            x1,
            y1,
            x2,
            y2,
            page: 1,
        }
    }

    fn rect(x: f32, y: f32, width: f32, height: f32) -> PdfRect {
        PdfRect {
            x,
            y,
            width,
            height,
            page: 1,
        }
    }

    /// The 1099-DIV "Account number" box as the extractor sees it: closed
    /// by rules at y=432 and y=468 and x=50.4 and x=237.6, with a small
    /// sub-box drawn in its top-right corner, beside the "2nd TIN not." box.
    fn account_box() -> (Vec<PdfRect>, Vec<PdfLine>) {
        let rects = vec![
            rect(0.0, 0.0, 612.0, 792.0),
            rect(165.6, 456.0, 72.0, 12.0),
            rect(262.2, 445.5, 9.0, 9.0),
            rect(50.4, 468.0, 187.2, 36.0),
        ];
        let lines = vec![
            line(49.9, 432.0, 238.0, 432.0),
            line(49.9, 468.0, 238.0, 468.0),
            line(165.2, 468.0, 238.0, 468.0),
            line(237.2, 432.0, 295.5, 432.0),
            line(237.2, 468.0, 295.5, 468.0),
            line(50.4, 468.2, 50.4, 431.5),
            line(237.6, 468.2, 237.6, 431.5),
            line(50.4, 504.4, 50.4, 467.6),
            line(237.6, 504.4, 237.6, 467.6),
        ];
        (rects, lines)
    }

    fn texts(items: &[TextItem]) -> Vec<&str> {
        items.iter().map(|item| item.text.as_str()).collect()
    }

    #[test]
    fn label_and_value_in_one_box_read_together() {
        // A rule elsewhere in the row splits a page-wide table grid between
        // this box's label and its value; as one item they stay together.
        let (rects, lines) = account_box();
        let mut items = vec![
            text(
                "Account number (see instructions) ",
                54.4,
                461.0,
                108.8,
                7.0,
            ),
            text("2nd TIN not. ", 247.0, 460.0, 40.7, 7.0),
            text("0000-123456", 120.4, 441.2, 47.2, 8.0),
        ];
        merge_boxed_text(&mut items, &rects, &lines);

        assert_eq!(
            texts(&items),
            [
                "Account number (see instructions) 0000-123456",
                "2nd TIN not. "
            ]
        );
        // Read at the label's line, across the box's text.
        assert_eq!((items[0].x, items[0].y), (54.4, 461.0));
        assert!((items[0].x + items[0].width - 167.6).abs() < 0.01);
    }

    #[test]
    fn shading_band_inside_a_box_is_not_a_rule() {
        // 1099-R shades the account box's entry area with a filled band
        // (no stroke) between the label and the value.
        let (mut rects, lines) = account_box();
        rects.push(rect(50.4, 444.0, 187.2, 12.0));
        let mut items = vec![
            text("Account number (see instructions)", 54.4, 460.0, 106.8, 7.0),
            text("IRA-000000", 122.9, 434.2, 43.2, 8.0),
        ];
        merge_boxed_text(&mut items, &rects, &lines);

        assert_eq!(
            texts(&items),
            ["Account number (see instructions) IRA-000000"]
        );
    }

    #[test]
    fn rule_between_lines_keeps_them_apart() {
        // A ruled table: header and data rows are separate boxes.
        let lines = vec![
            line(50.0, 500.0, 250.0, 500.0),
            line(50.0, 480.0, 250.0, 480.0),
            line(50.0, 460.0, 250.0, 460.0),
            line(50.0, 500.0, 50.0, 460.0),
            line(250.0, 500.0, 250.0, 460.0),
        ];
        let mut items = vec![
            text("Payer", 54.0, 486.0, 30.0, 8.0),
            text("Northgate", 54.0, 466.0, 40.0, 8.0),
        ];
        merge_boxed_text(&mut items, &[], &lines);

        assert_eq!(texts(&items), ["Payer", "Northgate"]);
    }

    #[test]
    fn tall_bordered_block_is_left_alone() {
        // A bordered paragraph is not a form box.
        let lines = vec![
            line(50.0, 700.0, 550.0, 700.0),
            line(50.0, 560.0, 550.0, 560.0),
            line(50.0, 700.0, 50.0, 560.0),
            line(550.0, 700.0, 550.0, 560.0),
        ];
        let mut items: Vec<TextItem> = (0..10)
            .map(|row| {
                text(
                    "A line of boxed prose",
                    54.0,
                    690.0 - row as f32 * 12.0,
                    90.0,
                    9.0,
                )
            })
            .collect();
        merge_boxed_text(&mut items, &[], &lines);

        assert_eq!(items.len(), 10);
    }

    #[test]
    fn wrapped_cell_text_in_one_face_is_left_alone() {
        // A ruled data table's cell wraps its prose over lines set in one
        // face; a form box's value is filled in a face of its own.
        let lines = vec![
            line(50.0, 500.0, 250.0, 500.0),
            line(50.0, 460.0, 250.0, 460.0),
            line(50.0, 500.0, 50.0, 460.0),
            line(250.0, 500.0, 250.0, 460.0),
        ];
        let mut items = vec![
            text("Select document type to", 54.0, 488.0, 120.0, 8.0),
            text("automatically run project", 54.0, 478.0, 120.0, 8.0),
            text("creation and deployment", 54.0, 468.0, 120.0, 8.0),
        ];
        merge_boxed_text(&mut items, &[], &lines);

        assert_eq!(items.len(), 3);
    }

    #[test]
    fn superscript_marker_is_not_a_filled_value() {
        // A ruled caption cell whose first line ends in a raised footnote
        // marker: the marker belongs to its line, and the cell's lines are
        // all one face.
        let lines = vec![
            line(130.0, 455.0, 460.0, 455.0),
            line(130.0, 428.0, 460.0, 428.0),
            line(130.0, 455.0, 130.0, 428.0),
            line(460.0, 455.0, 460.0, 428.0),
        ];
        let mut marker = text("74", 402.0, 445.4, 7.0, 6.0);
        marker.font = "F2".to_string();
        let mut items = vec![
            text(
                "Participation of Institutions in the VNR Meeting of",
                140.0,
                442.0,
                260.0,
                10.0,
            ),
            marker,
            text("Indonesia 2021.", 140.0, 431.0, 70.0, 10.0),
        ];
        merge_boxed_text(&mut items, &[], &lines);

        assert_eq!(items.len(), 3);
    }

    #[test]
    fn subscript_does_not_set_a_lines_face() {
        // A table header cell: "Liquid" over the symbol v with subscript f,
        // all in the header's font; only the subscript is smaller.
        let lines = vec![
            line(140.0, 672.0, 180.0, 672.0),
            line(140.0, 646.0, 180.0, 646.0),
            line(140.0, 672.0, 140.0, 646.0),
            line(180.0, 672.0, 180.0, 646.0),
        ];
        for (symbol, subscript) in [("v", "f"), ("H", "fg")] {
            let mut items = vec![
                text("Liquid", 143.9, 662.4, 20.0, 7.92),
                text(symbol, 153.4, 652.4, 4.4, 7.92),
                text(subscript, 157.8, 650.9, 3.0, 5.28),
            ];
            merge_boxed_text(&mut items, &[], &lines);

            assert_eq!(items.len(), 3, "{symbol}_{subscript}");
        }
    }

    #[test]
    fn box_open_below_is_left_alone() {
        let lines = vec![
            line(50.0, 500.0, 250.0, 500.0),
            line(50.0, 500.0, 50.0, 440.0),
            line(250.0, 500.0, 250.0, 440.0),
        ];
        let mut items = vec![
            text("Payer", 54.0, 490.0, 30.0, 8.0),
            text("Northgate", 54.0, 470.0, 40.0, 8.0),
        ];
        merge_boxed_text(&mut items, &[], &lines);

        assert_eq!(items.len(), 2);
    }

    #[test]
    fn form_field_values_keep_their_own_placement() {
        let (rects, lines) = account_box();
        let mut value = text("0000-123456", 120.4, 441.2, 47.2, 8.0);
        value.item_type = ItemType::FormField;
        let mut items = vec![
            text(
                "Account number (see instructions) ",
                54.4,
                461.0,
                108.8,
                7.0,
            ),
            value,
        ];
        merge_boxed_text(&mut items, &rects, &lines);

        assert_eq!(items.len(), 2);
    }

    #[test]
    fn style_survives_only_when_every_part_shares_it() {
        let (rects, lines) = account_box();
        let mut label = text(
            "Account number (see instructions) ",
            54.4,
            461.0,
            108.8,
            7.0,
        );
        label.is_bold = true;
        let mut items = vec![label, text("0000-123456", 120.4, 441.2, 47.2, 8.0)];
        merge_boxed_text(&mut items, &rects, &lines);

        assert_eq!(items.len(), 1);
        assert!(!items[0].is_bold);
    }
}
