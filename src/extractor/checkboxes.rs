//! Checkbox marks in Yes/No answer columns.
//!
//! IRS-style question lists answer in a narrow column headed "Yes" and "No":
//! each row's tick carries no label of its own, so a bare `[x]` cannot say
//! which answer was ticked. The header it sits under is that label.

use crate::text_utils::{is_checkbox_mark, is_checkbox_token};
use crate::types::TextItem;

/// Baselines this close share a row.
const ROW_TOLERANCE: f32 = 3.0;

/// Give each checkbox token on one page its label, so later layout keeps the
/// two together: the option word just right of the box, else the Yes/No
/// header of the answer column the box sits in, else the label printed just
/// above a box that stands alone on its row.
pub(crate) fn attach_checkbox_labels(items: &mut Vec<TextItem>) {
    join_inline_labels(items);
    label_answer_column_checkboxes(items);
    join_labels_above(items);
}

/// A box set just under its label, alone on its row ("Check here if
/// retired" over its box), joins the end of that label. Left alone, it is
/// read with whatever else shares its height, often another field's entry.
fn join_labels_above(items: &mut Vec<TextItem>) {
    let mut joins = Vec::new();
    for (index, mark) in items.iter().enumerate() {
        if !is_checkbox_token(&mark.text) {
            continue;
        }
        // Anything leading up to the box on its own row (a question, its
        // dot leaders) makes the row the box's context.
        let led_up_to = items.iter().any(|item| {
            (item.y - mark.y).abs() <= ROW_TOLERANCE
                && item.x + item.width <= mark.x + 1.0
                && mark.x - (item.x + item.width) <= 72.0
                && !is_checkbox_mark(&item.text)
        });
        if led_up_to {
            continue;
        }
        let reach = mark.font_size.max(8.0) * 2.5;
        let label = items
            .iter()
            .enumerate()
            .filter(|(_, item)| {
                item.y > mark.y + ROW_TOLERANCE
                    && item.y - mark.y <= reach
                    && item.x <= mark.x
                    && item.x + item.width >= mark.x + mark.width
                    && is_word(&item.text)
            })
            .min_by(|(_, a), (_, b)| a.y.total_cmp(&b.y))
            .map(|(label, _)| label);
        if let Some(label) = label {
            joins.push((index, label));
        }
    }
    if joins.is_empty() {
        return;
    }
    let mut remove = vec![false; items.len()];
    for (mark, label) in joins {
        let mark_text = items[mark].text.trim().to_string();
        let joined = format!("{} {mark_text}", items[label].text.trim_end());
        items[label].text = joined;
        remove[mark] = true;
    }
    let mut index = 0;
    items.retain(|_| {
        index += 1;
        !remove[index - 1]
    });
}

/// Forms set a box just left of its option label ("[x] Single",
/// "Yes [x] No"). Fold each checkbox token into the word starting just right
/// of it, so no column edge or table-cell boundary can separate the two.
/// The joined item keeps the stream position of whichever came first.
fn join_inline_labels(items: &mut Vec<TextItem>) {
    let mut taken = vec![false; items.len()];
    let mut joins = Vec::new();
    for (mark, item) in items.iter().enumerate() {
        if !is_checkbox_token(&item.text) {
            continue;
        }
        if let Some(label) = inline_label(items, item).filter(|&label| !taken[label]) {
            taken[label] = true;
            joins.push((mark, label));
        }
    }
    if joins.is_empty() {
        return;
    }
    let mut remove = vec![false; items.len()];
    for (mark, label) in joins {
        let (box_x, box_text) = (items[mark].x, items[mark].text.trim().to_string());
        let label_item = items[label].clone();
        items[mark.min(label)] = TextItem {
            text: format!("{box_text} {}", label_item.text.trim_start()),
            x: box_x,
            width: label_item.x + label_item.width - box_x,
            ..label_item
        };
        remove[mark.max(label)] = true;
    }
    let mut index = 0;
    items.retain(|_| {
        index += 1;
        !remove[index - 1]
    });
}

/// The word starting just right of the mark on its row: its option label.
fn inline_label(items: &[TextItem], mark: &TextItem) -> Option<usize> {
    let right = mark.x + mark.width;
    let reach = (mark.font_size * 2.0).max(12.0);
    items
        .iter()
        .enumerate()
        .filter(|(_, item)| {
            (item.y - mark.y).abs() <= ROW_TOLERANCE
                && item.x >= right - 1.0
                && item.x - right <= reach
                && is_word(&item.text)
        })
        .min_by(|(_, a), (_, b)| a.x.total_cmp(&b.x))
        .map(|(index, _)| index)
}

/// Label each checkbox token left without an option word that sits under a
/// "Yes" or "No" column header: `[x]` becomes `[x] No`.
fn label_answer_column_checkboxes(items: &mut [TextItem]) {
    let labels: Vec<(usize, &'static str)> = items
        .iter()
        .enumerate()
        .filter(|(_, mark)| is_checkbox_token(&mark.text))
        .filter_map(|(index, mark)| answer_header_above(items, mark).map(|label| (index, label)))
        .collect();
    for (index, label) in labels {
        let mark = items[index].text.trim().to_string();
        items[index].text = format!("{mark} {label}");
    }
}

/// The answer named by the nearest item above the mark that spans its
/// centre, provided that item is a "Yes"/"No" header. Other marks and
/// punctuation in between are passed over; any other word blocks the match.
fn answer_header_above(items: &[TextItem], mark: &TextItem) -> Option<&'static str> {
    let centre = mark.x + mark.width / 2.0;
    let header = items
        .iter()
        .filter(|item| {
            item.y > mark.y + ROW_TOLERANCE
                && item.x <= centre
                && item.x + item.width >= centre
                && is_word(&item.text)
        })
        .min_by(|a, b| a.y.total_cmp(&b.y))?;
    let words: Vec<&str> = header
        .text
        .split_whitespace()
        .map(|word| word.trim_end_matches(['.', ':', ',']))
        .collect();
    match words.as_slice() {
        ["Yes"] => Some("Yes"),
        ["No"] => Some("No"),
        // Both headers drawn as one run: the mark's side of it decides.
        ["Yes", "No"] if centre < header.x + header.width / 2.0 => Some("Yes"),
        ["Yes", "No"] => Some("No"),
        _ => None,
    }
}

/// Text with at least one letter or digit that is not itself a checkbox.
fn is_word(text: &str) -> bool {
    !is_checkbox_mark(text) && text.chars().any(char::is_alphanumeric)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ItemType;

    fn item(text: &str, x: f32, y: f32, width: f32) -> TextItem {
        TextItem {
            text: text.to_string(),
            x,
            y,
            width,
            height: 7.0,
            font: String::new(),
            font_size: 7.0,
            page: 1,
            is_bold: false,
            is_italic: false,
            is_underline: false,
            is_strikeout: false,
            item_type: ItemType::Text,
            mcid: None,
        }
    }

    /// Schedule B-style page: headers over a right-margin answer column,
    /// question rows ending in dot leaders.
    fn answer_column_page(tick_x: f32) -> Vec<TextItem> {
        vec![
            item("Yes ", 535.6, 722.6, 18.5),
            item("No", 559.1, 722.6, 12.2),
            item(
                "Is the partnership a publicly traded partnership?",
                50.0,
                681.0,
                300.0,
            ),
            item(".", 516.0, 682.1, 2.0),
            item("[x]", tick_x, 681.2, 7.1),
        ]
    }

    #[test]
    fn tick_under_no_header_reads_as_no() {
        let mut items = answer_column_page(561.6);
        attach_checkbox_labels(&mut items);
        assert_eq!(items[4].text, "[x] No");
    }

    #[test]
    fn tick_under_yes_header_reads_as_yes() {
        let mut items = answer_column_page(540.0);
        attach_checkbox_labels(&mut items);
        assert_eq!(items[4].text, "[x] Yes");
    }

    #[test]
    fn unticked_box_takes_its_header_too() {
        let mut items = answer_column_page(540.0);
        items[4].text = "[ ]".to_string();
        attach_checkbox_labels(&mut items);
        assert_eq!(items[4].text, "[ ] Yes");
    }

    #[test]
    fn combined_header_run_splits_by_side() {
        let mut items = answer_column_page(561.6);
        items.drain(0..2);
        items.insert(0, item("Yes No", 535.6, 722.6, 36.0));
        attach_checkbox_labels(&mut items);
        assert_eq!(items[3].text, "[x] No");
    }

    #[test]
    fn tick_joins_the_label_to_its_right() {
        // "Yes [x] No" on one row: the option label sits right of the box.
        let mut items = vec![
            item("No", 556.0, 520.0, 12.0),
            item("Yes ", 529.2, 497.8, 16.4),
            item("[x]", 556.0, 499.1, 4.7),
            item("No", 565.2, 497.8, 10.8),
        ];
        attach_checkbox_labels(&mut items);
        let texts: Vec<&str> = items.iter().map(|item| item.text.as_str()).collect();
        assert_eq!(texts, ["No", "Yes ", "[x] No"]);
        // The joined item reads as its label does: baseline, start and end.
        assert_eq!(items[2].y, 497.8);
        assert_eq!(items[2].x, 556.0);
        assert_eq!(items[2].x + items[2].width, 565.2 + 10.8);
    }

    #[test]
    fn tick_joins_the_nearer_of_two_options() {
        // K-1 line G: a ruled column edge falls between the second box and
        // its label, so a loose tick landed in the first option's cell.
        let mut items = vec![
            item("G ", 40.5, 447.0, 7.3),
            item("General partner or LLC", 72.0, 446.6, 75.7),
            item("[x]", 181.6, 447.8, 4.9),
            item("Limited partner or other LLC", 194.4, 446.6, 110.0),
        ];
        attach_checkbox_labels(&mut items);
        let texts: Vec<&str> = items.iter().map(|item| item.text.as_str()).collect();
        assert_eq!(
            texts,
            [
                "G ",
                "General partner or LLC",
                "[x] Limited partner or other LLC"
            ]
        );
    }

    #[test]
    fn box_under_its_label_joins_that_label() {
        // Form 706 line 2b: the box sits just under "Check here if retired",
        // level with the neighbouring box's entry far to the left.
        let mut items = vec![
            item("Check here if retired", 489.5, 566.3, 72.5),
            item("Investor and philanthropist", 66.8, 554.2, 103.1),
            item("[x]", 521.0, 557.1, 4.8),
        ];
        attach_checkbox_labels(&mut items);
        let texts: Vec<&str> = items.iter().map(|item| item.text.as_str()).collect();
        assert_eq!(
            texts,
            ["Check here if retired [x]", "Investor and philanthropist"]
        );
    }

    #[test]
    fn answer_after_its_question_on_the_row_stays_put() {
        // "... checked ..... [x]": the question leads up to the box on its
        // own row, so the line above is not its label.
        let mut items = vec![
            item("Schedule K-3 is attached if", 472.0, 616.0, 86.5),
            item("checked .", 472.0, 606.0, 33.9),
            item(".", 552.0, 606.0, 1.9),
            item("[x]", 563.6, 605.3, 4.9),
        ];
        attach_checkbox_labels(&mut items);
        assert_eq!(items[3].text, "[x]");
    }

    #[test]
    fn distant_word_is_not_a_label() {
        let mut items = vec![
            item("[x]", 100.0, 500.0, 5.0),
            item("Total", 160.0, 500.0, 20.0),
        ];
        attach_checkbox_labels(&mut items);
        assert_eq!(items[0].text, "[x]");
        assert_eq!(items.len(), 2);
    }

    #[test]
    fn word_between_header_and_tick_blocks_the_label() {
        let mut items = answer_column_page(561.6);
        items.push(item("Total", 555.0, 700.0, 20.0));
        attach_checkbox_labels(&mut items);
        assert_eq!(items[4].text, "[x]");
    }
}
