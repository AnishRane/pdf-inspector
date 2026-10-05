//! Hyperlink and AcroForm field extraction.

use crate::text_utils::decode_text_string;
use crate::types::{ItemType, TextItem};
use lopdf::{Document, Object, ObjectId};
use std::collections::{HashMap, HashSet};

use super::fonts::{resolve_array, resolve_dict};
use super::get_number;
use log::debug;

/// Upper bound on the number of form-field nodes visited during a single
/// `extract_form_fields` pass. A crafted PDF can chain thousands of distinct
/// `/Kids` fields to blow the stack even without an outright reference cycle,
/// so we cap total traversal work in addition to detecting cycles.
const MAX_FORM_FIELD_NODES: usize = 100_000;

/// Upper bound on `/Kids` recursion depth. Real AcroForm hierarchies are only
/// a few levels deep (fields → child fields → widgets); a crafted PDF can chain
/// tens of thousands of distinct fields into a linear `/Kids` list that would
/// overflow the stack via depth-first recursion long before the node budget is
/// reached. This depth cap bounds the stack independently of total node count.
const MAX_FORM_FIELD_DEPTH: usize = 100;

/// Traversal budget for the AcroForm field walk. Bounds both the number of
/// distinct nodes visited *and* the total number of `/Fields`/`/Kids` entries
/// examined.
///
/// Counting `visited` alone is not enough: invalid entries (non-references) and
/// duplicate references never grow `visited`, so an oversized array full of them
/// would iterate to completion no matter how large. Charging every examined
/// entry against the same budget makes it a real cap on traversal work.
pub(crate) struct FieldWalkBudget {
    visited: HashSet<ObjectId>,
    examined: usize,
}

impl FieldWalkBudget {
    fn new() -> Self {
        Self {
            visited: HashSet::new(),
            examined: 0,
        }
    }

    /// True once the budget is spent; callers must stop iterating and recursing.
    fn exhausted(&self) -> bool {
        self.visited.len() >= MAX_FORM_FIELD_NODES || self.examined >= MAX_FORM_FIELD_NODES
    }
}

pub fn extract_page_links(doc: &Document, page_id: ObjectId, page_num: u32) -> Vec<TextItem> {
    let mut links = Vec::new();

    // Try to get the page dictionary
    if let Ok(page_dict) = doc.get_dictionary(page_id) {
        // Get Annots array
        let annots = if let Ok(annots_ref) = page_dict.get(b"Annots") {
            if let Ok(obj_ref) = annots_ref.as_reference() {
                doc.get_object(obj_ref)
                    .ok()
                    .and_then(|o| o.as_array().ok().cloned())
            } else {
                annots_ref.as_array().ok().cloned()
            }
        } else {
            None
        };

        if let Some(annots) = annots {
            for annot_ref in annots {
                // Get annotation dictionary
                let annot_dict = if let Ok(obj_ref) = annot_ref.as_reference() {
                    doc.get_dictionary(obj_ref).ok()
                } else {
                    annot_ref.as_dict().ok()
                };

                if let Some(annot_dict) = annot_dict {
                    // Check if this is a Link annotation
                    if let Ok(subtype) = annot_dict.get(b"Subtype") {
                        if let Ok(subtype_name) = subtype.as_name() {
                            if subtype_name != b"Link" {
                                continue;
                            }
                        }
                    }

                    // Get the Rect (position)
                    let rect = if let Ok(rect_obj) = annot_dict.get(b"Rect") {
                        if let Ok(rect_array) = rect_obj.as_array() {
                            if rect_array.len() >= 4 {
                                let x1 = get_number(&rect_array[0]).unwrap_or(0.0);
                                let y1 = get_number(&rect_array[1]).unwrap_or(0.0);
                                let x2 = get_number(&rect_array[2]).unwrap_or(0.0);
                                let y2 = get_number(&rect_array[3]).unwrap_or(0.0);
                                Some((x1, y1, x2 - x1, y2 - y1))
                            } else {
                                None
                            }
                        } else {
                            None
                        }
                    } else {
                        None
                    };

                    // Get the action (A dictionary) or Dest
                    let uri = extract_link_uri(doc, annot_dict);

                    if let (Some((x, y, width, height)), Some(url)) = (rect, uri) {
                        links.push(TextItem {
                            text: url.clone(),
                            x,
                            y,
                            width,
                            height,
                            font: String::new(),
                            font_size: 0.0,
                            page: page_num,
                            is_bold: false,
                            is_italic: false,
                            is_underline: false,
                            is_strikeout: false,
                            item_type: ItemType::Link(url),
                            mcid: None,
                        });
                    }
                }
            }
        }
    }

    links
}

/// Extract URI from a link annotation
pub(crate) fn extract_link_uri(doc: &Document, annot_dict: &lopdf::Dictionary) -> Option<String> {
    // Try to get the A (Action) dictionary
    if let Ok(action_ref) = annot_dict.get(b"A") {
        let action_dict = if let Ok(obj_ref) = action_ref.as_reference() {
            doc.get_dictionary(obj_ref).ok()
        } else {
            action_ref.as_dict().ok()
        };

        if let Some(action_dict) = action_dict {
            // Check for URI action
            if let Ok(uri_obj) = action_dict.get(b"URI") {
                if let Ok(uri_str) = uri_obj.as_str() {
                    return Some(String::from_utf8_lossy(uri_str).to_string());
                }
            }
        }
    }

    // Try Dest (named destination) - less common for external links
    // We'll skip this for now as it requires looking up named destinations

    None
}

/// Extract form field values from AcroForm dictionary.
///
/// Each visible widget yields its value laid out inside the widget's
/// `/Rect`, as a viewer would draw it: a text field's value (a multiline one
/// joined into one run), or `[x]` / `[ ]` for a checkbox or radio button.
/// Read-only fields count, since they still show their values; empty,
/// hidden, password, push-button and signature fields yield nothing. Field
/// names such as `topmostSubform[0].Page1[0].f1_14[0]` are internal plumbing
/// and never reach the text; `RUST_LOG=pdf_inspector::extractor::links=debug`
/// logs each one beside its value.
pub(crate) fn extract_form_fields(
    doc: &Document,
    page_map: &HashMap<ObjectId, u32>,
) -> Vec<TextItem> {
    let mut items = Vec::new();

    // Navigate: trailer -> /Root -> /AcroForm -> /Fields
    let root = match doc.trailer.get(b"Root") {
        Ok(root_ref) => match root_ref.as_reference() {
            Ok(r) => match doc.get_dictionary(r) {
                Ok(d) => d,
                Err(_) => return items,
            },
            Err(_) => return items,
        },
        Err(_) => return items,
    };

    let acroform = match root.get(b"AcroForm") {
        Ok(obj) => match resolve_dict(doc, obj) {
            Some(d) => d,
            None => return items,
        },
        Err(_) => return items,
    };

    // Borrow the array rather than cloning it: a crafted `/Fields` can be huge,
    // and cloning would pay an O(n) allocation/copy before the budget check
    // below can stop the work.
    let fields = match acroform.get(b"Fields") {
        Ok(obj) => match resolve_array(doc, obj) {
            Some(arr) => arr,
            None => return items,
        },
        Err(_) => return items,
    };
    if fields.is_empty() {
        return items;
    }
    let annotation_pages = annotation_page_map(doc, page_map);
    let page_boxes: HashMap<u32, PageBox> = page_map
        .iter()
        .filter_map(|(&page_id, &page)| super::get_page_box(doc, page_id).map(|b| (page, b)))
        .collect();
    let inherited = Inherited {
        appearance: acroform.get(b"DA").ok().and_then(|o| o.as_str().ok()),
        ..Inherited::default()
    };

    // Bound the walk so a crafted PDF cannot send us into unbounded recursion
    // via a `/Kids` cycle, a deep chain, or an oversized array of invalid or
    // duplicate entries.
    let mut budget = FieldWalkBudget::new();

    for field_obj in fields {
        // Stop once the budget is spent so a `/Fields` array wider than the
        // budget can't burn CPU iterating entries whose walk would no-op. Charge
        // every entry (including invalid ones) against the budget.
        if budget.exhausted() {
            break;
        }
        budget.examined += 1;
        if let Ok(field_ref) = field_obj.as_reference() {
            walk_form_fields(
                doc,
                field_ref,
                inherited,
                "",
                page_map,
                &annotation_pages,
                &page_boxes,
                &mut items,
                &mut budget,
                0,
            );
        }
    }

    items
}

/// Field attributes a widget inherits from its ancestor fields
/// (ISO 32000-1 12.7.3.1): field type, field flags, value and default
/// appearance string (`/DA`, which the AcroForm dictionary defaults).
#[derive(Clone, Copy, Default)]
pub(crate) struct Inherited<'a> {
    ft: Option<&'a [u8]>,
    flags: i64,
    value: Option<&'a Object>,
    appearance: Option<&'a [u8]>,
}

/// A page's visible box: `(x0, y0, x1, y1)`.
type PageBox = (f32, f32, f32, f32);

// Field flags (`/Ff`, ISO 32000-1 tables 226 and 228).
const FF_MULTILINE: i64 = 1 << 12;
const FF_PASSWORD: i64 = 1 << 13;
const FF_PUSHBUTTON: i64 = 1 << 16;
// Annotation flags (`/F`, ISO 32000-1 table 165): Hidden and NoView.
const F_NOT_SHOWN: i64 = (1 << 1) | (1 << 5);

/// Map widget annotation objects back to the page whose `/Annots` array owns
/// them. Some valid widgets omit `/P`, so the page tree is the only reliable
/// ownership signal available for page-filtered extraction.
fn annotation_page_map(
    doc: &Document,
    page_map: &HashMap<ObjectId, u32>,
) -> HashMap<ObjectId, u32> {
    let mut annotation_pages = HashMap::new();
    for (&page_id, &page_num) in page_map {
        let Some(annotations) = doc
            .get_dictionary(page_id)
            .ok()
            .and_then(|page| page.get(b"Annots").ok())
            .and_then(|annotations| resolve_array(doc, annotations))
        else {
            continue;
        };
        for annotation in annotations {
            if let Ok(annotation_id) = annotation.as_reference() {
                annotation_pages.insert(annotation_id, page_num);
            }
        }
    }
    annotation_pages
}

/// Recursively walk the form field tree, extracting leaf field values.
#[allow(clippy::too_many_arguments)]
pub(crate) fn walk_form_fields<'a>(
    doc: &'a Document,
    field_id: ObjectId,
    inherited: Inherited<'a>,
    parent_name: &str,
    page_map: &HashMap<ObjectId, u32>,
    annotation_pages: &HashMap<ObjectId, u32>,
    page_boxes: &HashMap<u32, PageBox>,
    items: &mut Vec<TextItem>,
    budget: &mut FieldWalkBudget,
    depth: usize,
) {
    // Guard against `/Kids` cycles and pathologically large field trees.
    // Exceeding the depth cap means the chain is too deep to be a legitimate
    // form (and would overflow the stack); an exhausted budget means the tree is
    // too large. Both checks run *before* inserting so the visited set can never
    // grow past the budget.
    if depth > MAX_FORM_FIELD_DEPTH || budget.exhausted() {
        return;
    }
    // Revisiting an object ID means we hit a `/Kids` cycle.
    if !budget.visited.insert(field_id) {
        return;
    }

    let field_dict = match doc.get_dictionary(field_id) {
        Ok(d) => d,
        Err(_) => return,
    };

    let full_name = qualified_field_name(field_dict, parent_name);

    // Type, flags and value may each be inherited from a parent.
    let integer = |key: &[u8]| {
        field_dict
            .get(key)
            .ok()
            .and_then(|o| doc.dereference(o).ok())
            .and_then(|(_, o)| o.as_i64().ok())
    };
    let inherited = Inherited {
        ft: field_dict
            .get(b"FT")
            .ok()
            .and_then(|o| o.as_name().ok())
            .or(inherited.ft),
        flags: integer(b"Ff").unwrap_or(inherited.flags),
        value: field_dict
            .get(b"V")
            .ok()
            .and_then(|o| doc.dereference(o).ok())
            .map(|(_, o)| o)
            .or(inherited.value),
        appearance: field_dict
            .get(b"DA")
            .ok()
            .and_then(|o| o.as_str().ok())
            .or(inherited.appearance),
    };

    // Check for /Kids — if present, recurse into children
    if let Ok(kids_obj) = field_dict.get(b"Kids") {
        // Iterate the borrowed array directly — cloning a crafted, oversized
        // `/Kids` would allocate and copy every entry before the budget check
        // below could stop the work.
        if let Some(kids) = resolve_array(doc, kids_obj) {
            for kid in kids {
                // Stop once the budget is spent so a `/Kids` array wider than the
                // budget can't burn CPU iterating entries whose walk would no-op.
                // Charge every entry (including invalid/duplicate ones) against
                // the budget so this is a true traversal-work cap.
                if budget.exhausted() {
                    break;
                }
                budget.examined += 1;
                if let Ok(kid_ref) = kid.as_reference() {
                    walk_form_fields(
                        doc,
                        kid_ref,
                        inherited,
                        &full_name,
                        page_map,
                        annotation_pages,
                        page_boxes,
                        items,
                        budget,
                        depth + 1,
                    );
                }
            }
            return;
        }
    }

    // Leaf: a widget (or a field merged with its widget).
    let Some(ft) = inherited.ft else {
        return;
    };
    let flags = inherited.flags;
    let annotation_flags = integer(b"F").unwrap_or(0);
    // Read-only fields still show their values (a form locked after
    // signing marks every field read-only); hidden ones do not.
    if annotation_flags & F_NOT_SHOWN != 0 {
        return;
    }

    let text: String = match ft {
        b"Tx" | b"Ch" => {
            if ft == b"Tx" && flags & FF_PASSWORD != 0 {
                return;
            }
            // A text value, or a choice field's selected option(s).
            let value = match inherited.value {
                Some(Object::String(s, _)) => decode_text_string(s),
                Some(Object::Array(arr)) => arr
                    .iter()
                    .filter_map(|o| match o {
                        Object::String(s, _) => Some(decode_text_string(s)),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join(", "),
                _ => return,
            };
            // One item per field: a multiline value (a payer's name and
            // address) reads as one run, its lines joined by commas, so no
            // neighbouring column's text can land between them.
            let mut joined = String::new();
            for line in value.split(['\r', '\n']).map(str::trim) {
                if line.is_empty() {
                    continue;
                }
                if !joined.is_empty() {
                    joined.push_str(if joined.ends_with([',', ';']) {
                        " "
                    } else {
                        ", "
                    });
                }
                joined.push_str(line);
            }
            joined
        }
        b"Btn" => {
            if flags & FF_PUSHBUTTON != 0 {
                return;
            }
            if is_checked(doc, field_dict, inherited.value) {
                "[x]"
            } else {
                "[ ]"
            }
            .to_string()
        }
        _ => return,
    };
    if text.is_empty() {
        return;
    }

    // Get Rect for positioning
    let (x, y, width, height) = match field_dict.get(b"Rect") {
        Ok(rect_obj) => match rect_obj.as_array() {
            Ok(rect_array) if rect_array.len() >= 4 => {
                let x1 = get_number(&rect_array[0]).unwrap_or(0.0);
                let y1 = get_number(&rect_array[1]).unwrap_or(0.0);
                let x2 = get_number(&rect_array[2]).unwrap_or(0.0);
                let y2 = get_number(&rect_array[3]).unwrap_or(0.0);
                (x1.min(x2), y1.min(y2), (x2 - x1).abs(), (y2 - y1).abs())
            }
            _ => (0.0, 0.0, 0.0, 0.0),
        },
        Err(_) => (0.0, 0.0, 0.0, 0.0),
    };

    // Determine page number from /P reference
    let page_num = field_dict
        .get(b"P")
        .ok()
        .and_then(|o| o.as_reference().ok())
        .and_then(|p| page_map.get(&p).copied())
        .or_else(|| annotation_pages.get(&field_id).copied())
        .unwrap_or(1);

    // A value no reader can see must not read as printed text: a widget with
    // no area or off the page, or text inked white or set too small to read.
    let off_page = page_boxes
        .get(&page_num)
        .is_some_and(|&(x0, y0, x1, y1)| x + width <= x0 || x >= x1 || y + height <= y0 || y >= y1);
    if width < 0.5 || height < 0.5 || off_page || inherited.appearance.is_some_and(hides_text) {
        return;
    }

    debug!(
        "form field {:?} = {:?} on page {} at ({:.1}, {:.1}, {:.1}x{:.1})",
        full_name, text, page_num, x, y, width, height
    );

    // The value spans its widget box, which is where the form shows it and
    // what table cells and columns should see; its baseline sits where a
    // viewer draws it: a single line centred vertically, multiline text from
    // the top. A comb field's characters spread over the box all the same.
    let multiline = flags & FF_MULTILINE != 0;
    let size = if multiline {
        8.0
    } else {
        (height * 0.6).clamp(6.0, 10.0)
    };
    let baseline = if multiline {
        y + height - 2.0 - size * 0.8
    } else {
        y + ((height - size) / 2.0).max(0.0) + size * 0.25
    };
    items.push(TextItem {
        text,
        x,
        y: baseline,
        width,
        height,
        font: String::new(),
        font_size: size,
        page: page_num,
        is_bold: false,
        is_italic: false,
        is_underline: false,
        is_strikeout: false,
        item_type: ItemType::FormField,
        mcid: None,
    });
}

/// Whether a checkbox or radio widget shows its check.
///
/// The field value `/V` decides: the widget is ticked when `/V` names its
/// own on-state (the non-`Off` appearance in `/AP /N`, else its `/AS`). The
/// appearance state `/AS` is what a viewer regenerates from `/V`, so when
/// the two disagree (`/V /Off` with `/AS /Yes`, a stale appearance) the
/// value wins. Without a `/V`, `/AS` alone decides.
fn is_checked(doc: &Document, widget: &lopdf::Dictionary, value: Option<&Object>) -> bool {
    let appearance = widget.get(b"AS").ok().and_then(|o| o.as_name().ok());
    let on_state: Option<Vec<u8>> = widget
        .get(b"AP")
        .ok()
        .and_then(|ap| resolve_dict(doc, ap))
        .and_then(|ap| ap.get(b"N").ok())
        .and_then(|normal| resolve_dict(doc, normal))
        .and_then(|normal| {
            normal
                .iter()
                .map(|(name, _)| name)
                .find(|name| name.as_slice() != b"Off")
                .cloned()
        })
        .or_else(|| {
            appearance
                .filter(|name| *name != b"Off")
                .map(<[u8]>::to_vec)
        });
    match value.and_then(|v| v.as_name().ok()) {
        Some(b"Off") => false,
        Some(value) => match on_state {
            Some(on) => on.as_slice() == value,
            // No on-state is known and the appearance is Off: a radio kid
            // other than the chosen one.
            None => appearance.is_none(),
        },
        None => appearance.is_some_and(|name| name != b"Off"),
    }
}

/// Whether a default appearance string (`/DA`, e.g. `/Helv 10 Tf 0 g`) draws
/// text no reader can see: inked white, or set under 2pt (size 0 means
/// auto-sized, which is readable).
fn hides_text(appearance: &[u8]) -> bool {
    let text = String::from_utf8_lossy(appearance);
    let tokens: Vec<&str> = text.split_whitespace().collect();
    let numbers_before = |at: usize, count: usize| -> Option<Vec<f32>> {
        (at >= count)
            .then(|| {
                tokens[at - count..at]
                    .iter()
                    .map(|t| t.parse().ok())
                    .collect()
            })
            .flatten()
    };
    // The last colour set is the one the text is drawn in.
    let (mut tiny, mut white) = (false, false);
    for (at, token) in tokens.iter().enumerate() {
        match *token {
            "Tf" => {
                if let Some(size) = numbers_before(at, 1) {
                    tiny = size[0] > 0.0 && size[0] < 2.0;
                }
            }
            "g" => {
                if let Some(gray) = numbers_before(at, 1) {
                    white = gray[0] >= 0.99;
                }
            }
            "rg" => {
                if let Some(rgb) = numbers_before(at, 3) {
                    white = rgb.iter().all(|&c| c >= 0.99);
                }
            }
            "k" => {
                if let Some(cmyk) = numbers_before(at, 4) {
                    white = cmyk.iter().all(|&c| c <= 0.01);
                }
            }
            _ => {}
        }
    }
    tiny || white
}

/// The field's fully qualified name: its ancestors' partial names and its
/// own `/T`, joined by periods (ISO 32000-1 12.7.3.2). `/T` is a text
/// string, often UTF-16BE on IRS forms.
fn qualified_field_name(field_dict: &lopdf::Dictionary, parent_name: &str) -> String {
    let local_name = field_dict
        .get(b"T")
        .ok()
        .and_then(|o| o.as_str().ok())
        .map(decode_text_string)
        .unwrap_or_default();
    if parent_name.is_empty() {
        local_name
    } else if local_name.is_empty() {
        parent_name.to_string()
    } else {
        format!("{}.{}", parent_name, local_name)
    }
}

/// Baselines this close share a row.
const ROW_TOLERANCE: f32 = 3.0;

/// Where a form value joins the page's content stream: just before or just
/// after the printed item at `index` on its row (snapped to its baseline),
/// just after the label printed above its box, or just after the nearest
/// text when no label is in reach.
enum Anchor {
    Before(usize, f32),
    After(usize, f32),
    Under(usize),
    Near(usize),
}

/// Splice one page's form-field values into its content-stream order, each
/// beside the printed text it belongs with.
///
/// Widget values are not part of the page's content stream, so appended to
/// it they were read after the whole page, far from their labels. Placed
/// here they read as the form would flattened: a value after the label to
/// its left on the same row ("1a 584000"), else after the label printed
/// above its box ("Last name" / "Halvorsen-Pryce"); a checkbox before the
/// option label to its right ("[x] Single"). A value takes the text size
/// of the label it is read with, so it never outranks that label and reads
/// as a heading.
pub(crate) fn place_form_items(items: &mut Vec<TextItem>, fields: Vec<TextItem>) {
    if fields.is_empty() {
        return;
    }
    let body_size = body_font_size(items);
    let mut before: HashMap<usize, Vec<TextItem>> = HashMap::new();
    let mut after: HashMap<usize, Vec<TextItem>> = HashMap::new();
    let mut unplaced = Vec::new();
    for mut field in fields {
        field.font_size = body_size;
        let Some(placement) = anchor(items, &field) else {
            unplaced.push(field);
            continue;
        };
        let (index, row_y, under) = match placement {
            Anchor::Before(index, y) | Anchor::After(index, y) => (index, Some(y), false),
            Anchor::Under(index) => (index, None, true),
            Anchor::Near(index) => (index, None, false),
        };
        let label = &items[index];
        field.y = row_y.unwrap_or(field.y);
        if label.font_size > 0.0 {
            field.font_size = label.font_size;
        }
        // The value covers its text, not the whole widget box, so a wide box
        // does not straddle the page's columns. Under a label it covers at
        // least the label too, sharing the label's column and table cell.
        if !crate::text_utils::is_checkbox_token(&field.text) {
            let text_width = field.text.chars().count() as f32 * field.font_size * 0.5;
            let floor = if under { label.width } else { 0.0 };
            field.width = text_width.max(floor).min(field.width);
        }
        let group = if matches!(placement, Anchor::Before(..)) {
            &mut before
        } else {
            &mut after
        };
        group.entry(index).or_default().push(field);
    }
    // Several values at one anchor read top to bottom, left to right.
    let reading_order = |a: &TextItem, b: &TextItem| b.y.total_cmp(&a.y).then(a.x.total_cmp(&b.x));
    let mut placed = Vec::with_capacity(items.len() + unplaced.len());
    for (index, item) in std::mem::take(items).into_iter().enumerate() {
        if let Some(mut group) = before.remove(&index) {
            group.sort_by(reading_order);
            placed.extend(group);
        }
        placed.push(item);
        if let Some(mut group) = after.remove(&index) {
            group.sort_by(reading_order);
            placed.extend(group);
        }
    }
    unplaced.sort_by(reading_order);
    placed.extend(unplaced);
    *items = placed;
}

/// The most common text size on the page.
fn body_font_size(items: &[TextItem]) -> f32 {
    let mut counts: HashMap<u32, usize> = HashMap::new();
    for item in items.iter().filter(|item| is_printed_text(item)) {
        *counts
            .entry((item.font_size * 2.0).round() as u32)
            .or_default() += 1;
    }
    counts
        .into_iter()
        .max_by_key(|&(size, count)| (count, size))
        .map(|(size, _)| size as f32 / 2.0)
        .filter(|&size| size > 0.0)
        .unwrap_or(10.0)
}

/// Printed words a value can be read beside.
fn is_printed_text(item: &TextItem) -> bool {
    matches!(item.item_type, ItemType::Text) && item.text.chars().any(char::is_alphanumeric)
}

fn anchor(items: &[TextItem], field: &TextItem) -> Option<Anchor> {
    let right_edge = |item: &TextItem| item.x + item.width;
    let candidates = || {
        items
            .iter()
            .enumerate()
            .filter(|(_, item)| is_printed_text(item))
    };
    let same_row = |item: &TextItem| (item.y - field.y).abs() <= ROW_TOLERANCE;
    let left = candidates()
        .filter(|(_, item)| same_row(item) && right_edge(item) <= field.x + 2.0)
        .max_by(|(_, a), (_, b)| right_edge(a).total_cmp(&right_edge(b)));
    let right = candidates()
        .filter(|(_, item)| same_row(item) && item.x >= right_edge(field) - 2.0)
        .min_by(|(_, a), (_, b)| a.x.total_cmp(&b.x));

    // A checkbox's label is the option word just right of the box.
    if crate::text_utils::is_checkbox_token(&field.text) {
        let reach = (field.font_size * 2.0).max(12.0);
        if let Some((index, label)) = right.filter(|(_, r)| r.x - right_edge(field) <= reach) {
            return Some(Anchor::Before(index, label.y));
        }
    }
    if let Some((index, label)) = left {
        return Some(Anchor::After(index, label.y));
    }
    // The label printed above the box: the nearest text overlapping it.
    let reach_above = field.height + 3.0 * field.font_size;
    let above = candidates()
        .filter(|(_, item)| {
            item.y > field.y + ROW_TOLERANCE
                && item.y - field.y <= reach_above
                && item.x < right_edge(field)
                && right_edge(item) > field.x
        })
        .min_by(|(_, a), (_, b)| {
            a.y.total_cmp(&b.y)
                .then((a.x - field.x).abs().total_cmp(&(b.x - field.x).abs()))
        });
    if let Some((index, _)) = above {
        return Some(Anchor::Under(index));
    }
    if let Some((index, label)) = right {
        return Some(Anchor::Before(index, label.y));
    }
    // No label in reach: read it after the nearest text.
    let distance = |item: &TextItem| (item.x - field.x).powi(2) + (item.y - field.y).powi(2);
    candidates()
        .min_by(|(_, a), (_, b)| distance(a).total_cmp(&distance(b)))
        .map(|(index, _)| Anchor::Near(index))
}

#[cfg(test)]
mod tests {
    use super::*;
    use lopdf::{dictionary, Object, StringFormat};

    /// Build a PDF text string in UTF-16BE with the leading byte-order mark,
    /// the encoding ISO 32000-1 7.9.2.2 permits for any text string.
    fn utf16be(s: &str) -> Vec<u8> {
        let mut bytes = vec![0xFE, 0xFF];
        for unit in s.encode_utf16() {
            bytes.extend_from_slice(&unit.to_be_bytes());
        }
        bytes
    }

    fn single_field_doc(name: Object, value: Object) -> (Document, HashMap<ObjectId, u32>) {
        let mut doc = Document::new();
        let widget_id = doc.add_object(dictionary! {
            "Type" => "Annot",
            "Subtype" => "Widget",
            "FT" => "Tx",
            "T" => name,
            "V" => value,
            "Rect" => vec![10.into(), 20.into(), 110.into(), 40.into()],
        });
        let page_id = doc.add_object(dictionary! {
            "Type" => "Page",
            "Annots" => vec![Object::Reference(widget_id)],
        });
        let catalog_id = doc.add_object(dictionary! {
            "Type" => "Catalog",
            "AcroForm" => dictionary! {
                "Fields" => vec![Object::Reference(widget_id)],
            },
        });
        doc.trailer.set("Root", Object::Reference(catalog_id));
        (doc, HashMap::from([(page_id, 1)]))
    }

    #[test]
    fn utf16be_field_name_decodes_to_plain_text() {
        // Real IRS forms store `/T` as UTF-16BE. Decoding those bytes as UTF-8
        // interleaves a NUL between every ASCII character and turns the BOM
        // into U+FFFD, which is what leaked into extracted Markdown.
        let field = dictionary! {
            "T" => Object::String(utf16be("f1_14[0]"), StringFormat::Literal),
        };

        assert_eq!(
            qualified_field_name(&field, "topmostSubform[0].Page1[0]"),
            "topmostSubform[0].Page1[0].f1_14[0]"
        );
    }

    #[test]
    fn field_name_never_reaches_the_value_text() {
        let (doc, page_map) = single_field_doc(
            Object::String(
                utf16be("topmostSubform[0].Page1[0].f1_14[0]"),
                StringFormat::Literal,
            ),
            Object::string_literal("Alice"),
        );

        let items = extract_form_fields(&doc, &page_map);

        assert_eq!(items.len(), 1);
        assert_eq!(items[0].text, "Alice");
    }

    #[test]
    fn utf16be_field_value_decodes_to_plain_text() {
        let (doc, page_map) = single_field_doc(
            Object::string_literal("customer"),
            Object::String(utf16be("Zoë Ruiz"), StringFormat::Literal),
        );

        let items = extract_form_fields(&doc, &page_map);

        assert_eq!(items.len(), 1);
        assert_eq!(items[0].text, "Zoë Ruiz");
    }

    #[test]
    fn decoded_form_fields_carry_no_interior_nul_bytes() {
        // Guards the downstream symptom: a NUL makes the whole Markdown file
        // classify as binary, so `grep` and `file` stop treating it as text.
        let (doc, page_map) = single_field_doc(
            Object::String(utf16be("f1_59[0]"), StringFormat::Literal),
            Object::String(utf16be("14,653,649.00"), StringFormat::Literal),
        );

        let items = extract_form_fields(&doc, &page_map);

        assert_eq!(items.len(), 1);
        assert!(
            !items[0].text.contains('\0'),
            "form field text must not contain NUL: {:?}",
            items[0].text
        );
    }

    #[test]
    fn widget_without_page_reference_uses_owning_page_annotation() {
        let mut doc = Document::new();
        let widget_id = doc.add_object(dictionary! {
            "Type" => "Annot",
            "Subtype" => "Widget",
            "FT" => "Tx",
            "T" => Object::string_literal("customer"),
            "V" => Object::string_literal("Alice"),
            "Rect" => vec![10.into(), 20.into(), 110.into(), 40.into()],
        });
        let page_one_id = doc.add_object(dictionary! {
            "Type" => "Page",
        });
        let page_two_id = doc.add_object(dictionary! {
            "Type" => "Page",
            "Annots" => vec![Object::Reference(widget_id)],
        });
        let catalog_id = doc.add_object(dictionary! {
            "Type" => "Catalog",
            "AcroForm" => dictionary! {
                "Fields" => vec![Object::Reference(widget_id)],
            },
        });
        doc.trailer.set("Root", Object::Reference(catalog_id));

        let page_map = HashMap::from([(page_one_id, 1), (page_two_id, 2)]);
        let items = extract_form_fields(&doc, &page_map);

        assert_eq!(items.len(), 1);
        assert_eq!(items[0].page, 2);
        assert_eq!(items[0].text, "Alice");
    }

    #[test]
    fn kids_self_cycle_does_not_overflow_stack() {
        // A crafted AcroForm field that lists itself in `/Kids` must not send
        // the traversal into unbounded recursion.
        let mut doc = Document::new();
        let field_id = doc.new_object_id();
        doc.set_object(
            field_id,
            dictionary! {
                "FT" => "Tx",
                "T" => Object::string_literal("loop"),
                "Kids" => vec![Object::Reference(field_id)],
            },
        );
        let catalog_id = doc.add_object(dictionary! {
            "Type" => "Catalog",
            "AcroForm" => dictionary! {
                "Fields" => vec![Object::Reference(field_id)],
            },
        });
        doc.trailer.set("Root", Object::Reference(catalog_id));

        let page_map = HashMap::new();
        // Completes (rather than overflowing the stack) and yields no items.
        let items = extract_form_fields(&doc, &page_map);
        assert!(items.is_empty());
    }

    #[test]
    fn kids_mutual_cycle_terminates() {
        // Two fields that reference each other via `/Kids` form a cycle that
        // must also terminate.
        let mut doc = Document::new();
        let field_a = doc.new_object_id();
        let field_b = doc.new_object_id();
        doc.set_object(
            field_a,
            dictionary! {
                "T" => Object::string_literal("a"),
                "Kids" => vec![Object::Reference(field_b)],
            },
        );
        doc.set_object(
            field_b,
            dictionary! {
                "T" => Object::string_literal("b"),
                "Kids" => vec![Object::Reference(field_a)],
            },
        );
        let catalog_id = doc.add_object(dictionary! {
            "Type" => "Catalog",
            "AcroForm" => dictionary! {
                "Fields" => vec![Object::Reference(field_a)],
            },
        });
        doc.trailer.set("Root", Object::Reference(catalog_id));

        let page_map = HashMap::new();
        let items = extract_form_fields(&doc, &page_map);
        assert!(items.is_empty());
    }

    #[test]
    fn deep_acyclic_kids_chain_does_not_overflow_stack() {
        // A long chain of *distinct* fields (no cycle) must also terminate:
        // the visited set alone would still recurse to the chain length, so
        // the depth cap is what prevents a stack overflow here.
        let mut doc = Document::new();
        let n = MAX_FORM_FIELD_DEPTH * 500;
        let ids: Vec<ObjectId> = (0..=n).map(|_| doc.new_object_id()).collect();
        for i in 0..n {
            doc.set_object(
                ids[i],
                dictionary! {
                    "FT" => "Tx",
                    "Kids" => vec![Object::Reference(ids[i + 1])],
                },
            );
        }
        // Leaf carries a value; it sits far below the depth cap so it is never
        // reached, proving traversal stops early rather than crashing.
        doc.set_object(
            ids[n],
            dictionary! {
                "FT" => "Tx",
                "T" => Object::string_literal("leaf"),
                "V" => Object::string_literal("x"),
                "Rect" => vec![10.into(), 20.into(), 110.into(), 40.into()],
            },
        );
        let catalog_id = doc.add_object(dictionary! {
            "Type" => "Catalog",
            "AcroForm" => dictionary! {
                "Fields" => vec![Object::Reference(ids[0])],
            },
        });
        doc.trailer.set("Root", Object::Reference(catalog_id));

        let page_map = HashMap::new();
        let items = extract_form_fields(&doc, &page_map);
        assert!(items.is_empty());
    }

    #[test]
    fn wide_tree_traversal_stops_at_node_budget() {
        // A single field with a `/Kids` array wider than the node budget must
        // stop traversal at the cap rather than growing `visited` (and the work)
        // without bound. Each processed leaf emits one item, so the item count
        // is bounded by the budget and reaches right up to it (a couple of
        // slots go to the root and the boundary node charged against the cap).
        let mut doc = Document::new();
        let fanout = MAX_FORM_FIELD_NODES + 50;
        let leaf_ids: Vec<ObjectId> = (0..fanout).map(|_| doc.new_object_id()).collect();
        for &leaf in &leaf_ids {
            doc.set_object(
                leaf,
                dictionary! {
                    "FT" => "Tx",
                    "V" => Object::string_literal("v"),
                    "Rect" => vec![10.into(), 20.into(), 110.into(), 40.into()],
                },
            );
        }
        let kids: Vec<Object> = leaf_ids.iter().map(|&id| Object::Reference(id)).collect();
        let root_id = doc.add_object(dictionary! {
            "T" => Object::string_literal("root"),
            "Kids" => kids,
        });
        let catalog_id = doc.add_object(dictionary! {
            "Type" => "Catalog",
            "AcroForm" => dictionary! {
                "Fields" => vec![Object::Reference(root_id)],
            },
        });
        doc.trailer.set("Root", Object::Reference(catalog_id));

        let page_map = HashMap::new();
        let items = extract_form_fields(&doc, &page_map);
        // Extraction stops at the budget: bounded above by the cap, and it gets
        // right up to it (allowing a small delta for the root/boundary nodes
        // charged against the budget).
        assert!(items.len() <= MAX_FORM_FIELD_NODES);
        assert!(items.len() >= MAX_FORM_FIELD_NODES - 3);
    }

    #[test]
    fn wide_top_level_fields_stop_at_node_budget() {
        // A top-level `/Fields` array wider than the budget must also stop at
        // the cap: the item count is bounded by the budget and reaches right up
        // to it.
        let mut doc = Document::new();
        let fanout = MAX_FORM_FIELD_NODES + 50;
        let leaf_ids: Vec<ObjectId> = (0..fanout).map(|_| doc.new_object_id()).collect();
        for &leaf in &leaf_ids {
            doc.set_object(
                leaf,
                dictionary! {
                    "FT" => "Tx",
                    "V" => Object::string_literal("v"),
                    "Rect" => vec![10.into(), 20.into(), 110.into(), 40.into()],
                },
            );
        }
        let fields: Vec<Object> = leaf_ids.iter().map(|&id| Object::Reference(id)).collect();
        let catalog_id = doc.add_object(dictionary! {
            "Type" => "Catalog",
            "AcroForm" => dictionary! {
                "Fields" => fields,
            },
        });
        doc.trailer.set("Root", Object::Reference(catalog_id));

        let page_map = HashMap::new();
        let items = extract_form_fields(&doc, &page_map);
        assert!(items.len() <= MAX_FORM_FIELD_NODES);
        assert!(items.len() >= MAX_FORM_FIELD_NODES - 3);
    }

    #[test]
    fn duplicate_and_invalid_kids_entries_stop_at_budget() {
        // Duplicate references and non-reference junk never grow `visited`, so
        // without charging examined entries against the budget an oversized
        // array of them would iterate to completion. The walk must still
        // terminate and extract the single real leaf exactly once.
        let mut doc = Document::new();
        let leaf_id = doc.new_object_id();
        doc.set_object(
            leaf_id,
            dictionary! {
                "FT" => "Tx",
                "V" => Object::string_literal("v"),
                "Rect" => vec![10.into(), 20.into(), 110.into(), 40.into()],
            },
        );
        // A `/Kids` array far wider than the budget: half duplicate references
        // to the same leaf, half invalid (null) entries.
        let mut kids: Vec<Object> = Vec::new();
        for i in 0..(MAX_FORM_FIELD_NODES * 2) {
            if i % 2 == 0 {
                kids.push(Object::Reference(leaf_id));
            } else {
                kids.push(Object::Null);
            }
        }
        let root_id = doc.add_object(dictionary! {
            "T" => Object::string_literal("root"),
            "Kids" => kids,
        });
        let catalog_id = doc.add_object(dictionary! {
            "Type" => "Catalog",
            "AcroForm" => dictionary! {
                "Fields" => vec![Object::Reference(root_id)],
            },
        });
        doc.trailer.set("Root", Object::Reference(catalog_id));

        let page_map = HashMap::new();
        let items = extract_form_fields(&doc, &page_map);
        assert_eq!(items.len(), 1);
    }

    /// One page holding the given widget dictionaries as AcroForm fields.
    fn form_doc(widgets: Vec<lopdf::Dictionary>) -> (Document, HashMap<ObjectId, u32>) {
        let mut doc = Document::new();
        let ids: Vec<ObjectId> = widgets.into_iter().map(|w| doc.add_object(w)).collect();
        let refs: Vec<Object> = ids.iter().map(|&id| Object::Reference(id)).collect();
        let page_id = doc.add_object(dictionary! {
            "Type" => "Page",
            "Annots" => refs.clone(),
        });
        let catalog_id = doc.add_object(dictionary! {
            "Type" => "Catalog",
            "AcroForm" => dictionary! { "Fields" => refs },
        });
        doc.trailer.set("Root", Object::Reference(catalog_id));
        (doc, HashMap::from([(page_id, 1)]))
    }

    fn texts(items: &[TextItem]) -> Vec<&str> {
        items.iter().map(|item| item.text.as_str()).collect()
    }

    #[test]
    fn read_only_field_keeps_its_value() {
        // A form locked after signing marks every field read-only; the
        // values still show on the page, and must still be read.
        let (doc, page_map) = form_doc(vec![dictionary! {
            "FT" => "Tx",
            "Ff" => 1,
            "T" => Object::string_literal("name"),
            "V" => Object::string_literal("Celeste W"),
            "Rect" => vec![10.into(), 20.into(), 110.into(), 40.into()],
        }]);

        assert_eq!(texts(&extract_form_fields(&doc, &page_map)), ["Celeste W"]);
    }

    #[test]
    fn hidden_password_and_empty_fields_emit_nothing() {
        let tx = |name: &str, value: &str| {
            dictionary! {
                "FT" => "Tx",
                "T" => Object::string_literal(name),
                "V" => Object::string_literal(value),
                "Rect" => vec![10.into(), 20.into(), 110.into(), 40.into()],
            }
        };
        let mut hidden = tx("hidden", "secret");
        hidden.set("F", 2);
        let mut no_view = tx("noview", "secret");
        no_view.set("F", 32);
        let mut password = tx("pin", "1234");
        password.set("Ff", 1 << 13);
        let (doc, page_map) = form_doc(vec![
            hidden,
            no_view,
            password,
            tx("empty", ""),
            tx("kept", "Alice"),
        ]);

        assert_eq!(texts(&extract_form_fields(&doc, &page_map)), ["Alice"]);
    }

    #[test]
    fn checkboxes_and_radios_read_as_ticked_or_unticked_boxes() {
        let btn = |name: &str, state: &str, ff: i64| {
            dictionary! {
                "FT" => "Btn",
                "Ff" => ff,
                "T" => Object::string_literal(name),
                "V" => Object::Name(state.as_bytes().to_vec()),
                "AS" => Object::Name(state.as_bytes().to_vec()),
                "Rect" => vec![10.into(), 20.into(), 18.into(), 28.into()],
            }
        };
        let (doc, page_map) = form_doc(vec![
            btn("single", "1", 0),
            btn("joint", "Off", 0),
            btn("yes", "Yes", 1 << 15),
            btn("print", "Off", 1 << 16),
        ]);

        assert_eq!(
            texts(&extract_form_fields(&doc, &page_map)),
            ["[x]", "[ ]", "[x]"]
        );
    }

    #[test]
    fn kid_widgets_show_their_parent_fields_value() {
        // A field placed twice: the value lives on the parent, the widgets
        // are its kids. Each widget shows the value, as on the page.
        let mut doc = Document::new();
        let kid = |doc: &mut Document, y: i64| {
            doc.add_object(dictionary! {
                "Type" => "Annot",
                "Subtype" => "Widget",
                "Rect" => vec![10.into(), y.into(), 110.into(), (y + 14).into()],
            })
        };
        let (a, b) = (kid(&mut doc, 700), kid(&mut doc, 300));
        let parent = doc.add_object(dictionary! {
            "FT" => "Tx",
            "T" => Object::string_literal("name"),
            "V" => Object::string_literal("Celeste W"),
            "Kids" => vec![Object::Reference(a), Object::Reference(b)],
        });
        let page_id = doc.add_object(dictionary! {
            "Type" => "Page",
            "Annots" => vec![Object::Reference(a), Object::Reference(b)],
        });
        let catalog_id = doc.add_object(dictionary! {
            "Type" => "Catalog",
            "AcroForm" => dictionary! { "Fields" => vec![Object::Reference(parent)] },
        });
        doc.trailer.set("Root", Object::Reference(catalog_id));

        let items = extract_form_fields(&doc, &HashMap::from([(page_id, 1)]));
        assert_eq!(texts(&items), ["Celeste W", "Celeste W"]);
    }

    #[test]
    fn radio_kids_tick_only_the_chosen_widget() {
        let mut doc = Document::new();
        let kid = |doc: &mut Document, x: i64, state: &str| {
            doc.add_object(dictionary! {
                "Type" => "Annot",
                "Subtype" => "Widget",
                "AS" => Object::Name(state.as_bytes().to_vec()),
                "Rect" => vec![x.into(), 20.into(), (x + 8).into(), 28.into()],
            })
        };
        let (yes, no) = (kid(&mut doc, 10, "Off"), kid(&mut doc, 60, "2"));
        let parent = doc.add_object(dictionary! {
            "FT" => "Btn",
            "Ff" => 1 << 15,
            "T" => Object::string_literal("c1_10"),
            "V" => Object::Name(b"2".to_vec()),
            "Kids" => vec![Object::Reference(yes), Object::Reference(no)],
        });
        let page_id = doc.add_object(dictionary! {
            "Type" => "Page",
            "Annots" => vec![Object::Reference(yes), Object::Reference(no)],
        });
        let catalog_id = doc.add_object(dictionary! {
            "Type" => "Catalog",
            "AcroForm" => dictionary! { "Fields" => vec![Object::Reference(parent)] },
        });
        doc.trailer.set("Root", Object::Reference(catalog_id));

        let items = extract_form_fields(&doc, &HashMap::from([(page_id, 1)]));
        assert_eq!(texts(&items), ["[ ]", "[x]"]);
    }

    #[test]
    fn multiline_value_reads_as_one_item() {
        let (doc, page_map) = form_doc(vec![dictionary! {
            "FT" => "Tx",
            "Ff" => 1 << 12,
            "T" => Object::string_literal("payer"),
            "V" => Object::string_literal("Ashbury Family Trust\r1450 Halsey Lane"),
            "Rect" => vec![36.into(), 600.into(), 300.into(), 640.into()],
        }]);

        // One item keeps a payer's name and address together; split into
        // lines, each joined whatever text shared its row in the next column.
        let items = extract_form_fields(&doc, &page_map);
        assert_eq!(texts(&items), ["Ashbury Family Trust, 1450 Halsey Lane"]);
        // Read from the box's first line.
        assert!(items[0].y > 620.0, "{}", items[0].y);
    }

    #[test]
    fn value_spans_its_widget_box() {
        // A right-aligned SSN: the value is laid over the whole box, which is
        // where the form shows it and what table cells and columns see.
        let (doc, page_map) = form_doc(vec![dictionary! {
            "FT" => "Tx",
            "Q" => 2,
            "T" => Object::string_literal("ssn"),
            "V" => Object::string_literal("987-65-4324"),
            "Rect" => vec![446.4.into(), 684.into(), 576.into(), 698.into()],
        }]);

        let items = extract_form_fields(&doc, &page_map);
        assert_eq!(texts(&items), ["987-65-4324"]);
        assert_eq!(items[0].x, 446.4);
        assert!((items[0].width - 129.6).abs() < 0.01, "{}", items[0].width);
    }

    /// A text widget on a 612x792 page, with `extra` entries set on it.
    fn visible_doc(extra: Vec<(&str, Object)>) -> (Document, HashMap<ObjectId, u32>) {
        let mut widget = dictionary! {
            "FT" => "Tx",
            "T" => Object::string_literal("field"),
            "V" => Object::string_literal("Your social security number"),
            "DA" => Object::string_literal("/Helv 10 Tf 0 g"),
            "Rect" => vec![36.into(), 684.into(), 251.into(), 698.into()],
        };
        for (key, value) in extra {
            widget.set(key, value);
        }
        let mut doc = Document::new();
        let id = doc.add_object(widget);
        let page_id = doc.add_object(dictionary! {
            "Type" => "Page",
            "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
            "Annots" => vec![Object::Reference(id)],
        });
        let catalog_id = doc.add_object(dictionary! {
            "Type" => "Catalog",
            "AcroForm" => dictionary! { "Fields" => vec![Object::Reference(id)] },
        });
        doc.trailer.set("Root", Object::Reference(catalog_id));
        (doc, HashMap::from([(page_id, 1)]))
    }

    #[test]
    fn widget_a_reader_can_see_keeps_its_value() {
        let (doc, page_map) = visible_doc(vec![]);
        assert_eq!(extract_form_fields(&doc, &page_map).len(), 1);
        // An auto-sized font (size 0) is drawn at a readable size.
        let (doc, page_map) = visible_doc(vec![("DA", Object::string_literal("/Helv 0 Tf 0 g"))]);
        assert_eq!(extract_form_fields(&doc, &page_map).len(), 1);
    }

    #[test]
    fn widgets_no_reader_can_see_emit_nothing() {
        // Values a person never sees must not read as printed text: a widget
        // off the page, one with no area, one inked white, one set too small
        // to read.
        let invisible = [
            (
                "Rect",
                vec![700.into(), 684.into(), 800.into(), 698.into()].into(),
            ),
            (
                "Rect",
                vec![36.into(), 684.into(), 36.into(), 698.into()].into(),
            ),
            ("DA", Object::string_literal("/Helv 10 Tf 1 g")),
            ("DA", Object::string_literal("/Helv 10 Tf 1 1 1 rg")),
            ("DA", Object::string_literal("/Helv 10 Tf 0 0 0 0 k")),
            ("DA", Object::string_literal("/Helv 1 Tf 0 g")),
            ("DA", Object::string_literal("1 g /Helv 10 Tf")),
        ];
        for (key, value) in invisible {
            let (doc, page_map) = visible_doc(vec![(key, value.clone())]);
            assert!(
                extract_form_fields(&doc, &page_map).is_empty(),
                "{key} {value:?} should hide the value"
            );
        }
    }

    #[test]
    fn checkbox_state_follows_the_field_value() {
        // /V is the field's value; /AS is the appearance a viewer should
        // regenerate from it. When they disagree, the value wins.
        let checkbox = |value: &str, appearance: &str| {
            dictionary! {
                "FT" => "Btn",
                "T" => Object::string_literal("box"),
                "V" => Object::Name(value.as_bytes().to_vec()),
                "AS" => Object::Name(appearance.as_bytes().to_vec()),
                "AP" => dictionary! { "N" => dictionary! { "Yes" => Object::Null, "Off" => Object::Null } },
                "Rect" => vec![10.into(), 20.into(), 18.into(), 28.into()],
            }
        };
        let (doc, page_map) = form_doc(vec![checkbox("Off", "Yes"), checkbox("Yes", "Off")]);
        assert_eq!(texts(&extract_form_fields(&doc, &page_map)), ["[ ]", "[x]"]);
    }

    // --- place_form_items ---

    fn text(text: &str, x: f32, y: f32, width: f32) -> TextItem {
        TextItem {
            text: text.to_string(),
            x,
            y,
            width,
            height: 8.0,
            font: "F1".to_string(),
            font_size: 8.0,
            page: 1,
            is_bold: false,
            is_italic: false,
            is_underline: false,
            is_strikeout: false,
            item_type: ItemType::Text,
            mcid: None,
        }
    }

    fn value(value: &str, x: f32, y: f32, width: f32, height: f32) -> TextItem {
        TextItem {
            item_type: ItemType::FormField,
            font_size: 0.0,
            height,
            ..text(value, x, y, width)
        }
    }

    fn order(items: &[TextItem]) -> Vec<&str> {
        items.iter().map(|item| item.text.as_str()).collect()
    }

    #[test]
    fn value_follows_the_label_printed_above_its_box() {
        // 1040 header: the labels sit above their boxes, and the content
        // stream draws the whole page before any widget value exists.
        let mut items = vec![
            text("Your first name and middle initial", 36.0, 700.0, 105.0),
            text("Last name", 256.0, 700.0, 35.0),
            text("If joint return, spouse's first name", 36.0, 676.0, 159.0),
            text("Home address", 36.0, 652.0, 60.0),
        ];
        place_form_items(
            &mut items,
            vec![
                value("Celeste W", 36.0, 684.0, 215.0, 14.0),
                value("Halvorsen-Pryce", 253.0, 684.0, 214.0, 14.0),
            ],
        );
        assert_eq!(
            order(&items),
            [
                "Your first name and middle initial",
                "Celeste W",
                "Last name",
                "Halvorsen-Pryce",
                "If joint return, spouse's first name",
                "Home address",
            ]
        );
        // Values take their label's text size rather than none at all.
        assert_eq!(items[1].font_size, 8.0);
    }

    #[test]
    fn value_is_never_larger_than_its_label() {
        // Most of the page is set at 9pt, the box label at 7pt: a value set
        // at the page size would outrank its label and read as a heading.
        let mut items = vec![text("Name(s) shown on return", 36.0, 700.3, 90.0)];
        items[0].font_size = 7.0;
        for row in 0..5 {
            let mut body = text(
                "Line text set in the body size",
                36.0,
                600.0 - row as f32 * 12.0,
                120.0,
            );
            body.font_size = 9.0;
            items.push(body);
        }
        place_form_items(
            &mut items,
            vec![value(
                "Celeste W. Halvorsen-Pryce",
                36.0,
                684.0,
                409.6,
                14.0,
            )],
        );

        assert_eq!(items[1].text, "Celeste W. Halvorsen-Pryce");
        assert_eq!(items[1].font_size, 7.0);
    }

    #[test]
    fn value_joins_the_row_of_the_label_to_its_left() {
        let mut items = vec![
            text("Total amount from Form(s) W-2", 108.0, 332.0, 200.0),
            text("1a", 489.0, 332.0, 9.0),
            text("Household employee wages", 108.0, 320.0, 200.0),
        ];
        place_form_items(&mut items, vec![value("584000", 504.0, 330.0, 72.0, 12.0)]);

        assert_eq!(
            order(&items),
            [
                "Total amount from Form(s) W-2",
                "1a",
                "584000",
                "Household employee wages"
            ]
        );
        // On the label's baseline, so line grouping keeps them together.
        assert_eq!(items[2].y, 332.0);
    }

    #[test]
    fn checkbox_goes_before_the_option_label_to_its_right() {
        let mut items = vec![
            text("Filing Status", 36.0, 577.0, 50.0),
            text("Single", 110.0, 578.3, 24.0),
            text("Head of household (HOH)", 362.0, 578.3, 92.0),
        ];
        place_form_items(
            &mut items,
            vec![
                value("[x]", 98.0, 578.0, 8.0, 8.0),
                value("[ ]", 350.0, 578.0, 8.0, 8.0),
            ],
        );
        assert_eq!(
            order(&items),
            [
                "Filing Status",
                "[x]",
                "Single",
                "[ ]",
                "Head of household (HOH)"
            ]
        );
    }

    #[test]
    fn value_under_a_label_is_as_wide_as_its_text_or_its_label() {
        // Form 4952's name box runs most of the page width. Laid over all of
        // it, the value straddled the header's columns and was read apart
        // from its label; at least as wide as the label, it shares the
        // label's column and table cell.
        let mut items = vec![
            text("Name(s) shown on return", 36.0, 700.0, 189.4),
            text("Identifying number", 464.8, 700.0, 64.5),
        ];
        place_form_items(
            &mut items,
            vec![
                value("Celeste W. Halvorsen-Pryce", 36.0, 688.9, 424.1, 14.0),
                value("987-65-4324", 460.8, 688.9, 115.2, 14.0),
            ],
        );
        assert_eq!(items[1].text, "Celeste W. Halvorsen-Pryce");
        assert_eq!((items[1].x, items[1].width), (36.0, 189.4));
        assert_eq!(items[3].text, "987-65-4324");
        assert_eq!((items[3].x, items[3].width), (460.8, 64.5));
    }

    #[test]
    fn value_beside_a_row_label_is_as_wide_as_its_text() {
        let mut items = vec![text("1a", 489.0, 332.0, 9.0)];
        place_form_items(&mut items, vec![value("584000", 504.0, 330.0, 72.0, 12.0)]);
        assert_eq!(items[1].x, 504.0);
        assert_eq!(items[1].width, 6.0 * 8.0 * 0.5);
    }

    #[test]
    fn value_on_a_page_without_text_is_kept() {
        let mut items = Vec::new();
        place_form_items(&mut items, vec![value("Alice", 36.0, 684.0, 215.0, 14.0)]);
        assert_eq!(order(&items), ["Alice"]);
    }
}
