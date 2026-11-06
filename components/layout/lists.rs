/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use layout_api::{LayoutElement, LayoutNode};
use script::layout_dom::ServoLayoutNode;
use style::counter_style::{CounterStyle, Symbol, SymbolsType};
use style::properties::longhands::list_style_type::computed_value::T as ListStyleType;
use style::values::computed::Image;
use style::values::generics::counters::Content;
use stylo_atoms::atom;
use web_atoms::{local_name, ns};

use crate::context::LayoutContext;
use crate::dom_traversal::{
    NodeAndStyleInfo, PseudoElementContentItem, generate_pseudo_element_content,
};
use crate::replaced::ReplacedContents;

/// <https://drafts.csswg.org/css-lists/#content-property>
pub(crate) fn make_marker<'dom>(
    context: &LayoutContext,
    info: &NodeAndStyleInfo<'dom>,
) -> Option<(NodeAndStyleInfo<'dom>, Vec<PseudoElementContentItem>)> {
    let marker_info =
        info.with_pseudo_element(context, style::selector_parser::PseudoElement::Marker)?;
    let style = &marker_info.style;
    let list_style = style.get_list();

    // https://drafts.csswg.org/css-lists/#marker-image
    let marker_image = || match &list_style.list_style_image {
        Image::Url(url) => Some(vec![
            PseudoElementContentItem::Replaced(ReplacedContents::from_image_url(
                marker_info.node,
                context,
                url,
            )?),
            PseudoElementContentItem::Text(" ".into()),
        ]),
        // XXX: Non-None image types unimplemented.
        Image::ImageSet(..) |
        Image::Gradient(..) |
        Image::Image(..) |
        Image::CrossFade(..) |
        Image::PaintWorklet(..) |
        Image::None => None,
        Image::LightDark(..) => unreachable!("light-dark() should be disabled"),
    };

    let content = match &marker_info.style.get_counters().content {
        Content::Items(_) => generate_pseudo_element_content(&marker_info, context),
        Content::None => return None,
        Content::Normal => marker_image().or_else(|| {
            Some(vec![PseudoElementContentItem::Text(marker_string(
                &list_style.list_style_type,
                list_item_ordinal(context, info.node),
            )?)])
        })?,
    };

    Some((marker_info, content))
}

/// The value of the `list-item` counter for the list item `node`, following the HTML
/// [ordinal value] rules: list items are numbered among their siblings, starting from the
/// parent `<ol>`'s `start` (or the item count when it is `reversed`) and honoring `<li value>`.
///
/// This stands in for CSS counters, which aren't implemented yet, so that ordered lists
/// aren't all numbered 0. `counter-reset`/`counter-increment` are not taken into account.
///
/// [ordinal value]: https://html.spec.whatwg.org/multipage/#ordinal-value
pub(crate) fn list_item_ordinal(context: &LayoutContext, node: ServoLayoutNode<'_>) -> i32 {
    #[expect(unsafe_code)]
    let Some(parent) = (unsafe { node.dangerous_dom_parent() }) else {
        return 1;
    };
    let is_list_item = |child: &ServoLayoutNode<'_>| {
        child.as_element().is_some_and(|element| {
            element
                .style_data()
                .is_some_and(|data| data.element_data.borrow().has_styles()) &&
                element
                    .style(&context.style_context)
                    .get_box()
                    .display
                    .is_list_item()
        })
    };

    let parent_element = parent.as_element();
    let is_ordered_list = parent_element.is_some_and(|element| {
        element.is_html_element_in_html_document() && *element.local_name() == local_name!("ol")
    });
    let reversed = is_ordered_list &&
        parent_element.is_some_and(|element| {
            element
                .attribute(&ns!(), &local_name!("reversed"))
                .is_some()
        });
    let start = parent_element
        .filter(|_| is_ordered_list)
        .and_then(|element| {
            element
                .attribute_as_str(&ns!(), &local_name!("start"))?
                .trim()
                .parse::<i32>()
                .ok()
        });
    let start = start.unwrap_or_else(|| {
        if reversed {
            parent.dom_children().filter(is_list_item).count() as i32
        } else {
            1
        }
    });
    let step = if reversed { -1 } else { 1 };

    let target = node.opaque();
    let mut ordinal: Option<i32> = None;
    for child in parent.dom_children().filter(is_list_item) {
        let value = child
            .as_element()
            .filter(|element| *element.local_name() == local_name!("li"))
            .and_then(|element| {
                element
                    .attribute_as_str(&ns!(), &local_name!("value"))?
                    .trim()
                    .parse::<i32>()
                    .ok()
            });
        ordinal = Some(match (value, ordinal) {
            (Some(value), _) => value,
            (None, None) => start,
            (None, Some(previous)) => previous.saturating_add(step),
        });
        if child.opaque() == target {
            break;
        }
    }
    ordinal.unwrap_or(start)
}

fn symbol_to_string(symbol: &Symbol) -> &str {
    match symbol {
        Symbol::String(string) => string,
        Symbol::Ident(ident) => &ident.0,
    }
}

/// <https://drafts.csswg.org/css-counter-styles-3/#generate-a-counter>
///
/// Only the predefined counter styles that are common on the web are generated exactly;
/// the other complex predefined styles (for example the CJK and Korean ones) fall back
/// to `decimal`.
pub(crate) fn generate_counter_representation(counter_style: &CounterStyle, value: i32) -> String {
    match counter_style {
        CounterStyle::None | CounterStyle::String(_) => unreachable!("Invalid counter style"),
        CounterStyle::Name(name) => {
            let digits = |zero: char| numeric(value, &decimal_digits(zero));
            match name.0 {
                atom!("disc") => "\u{2022}".into(),            /* "•" */
                atom!("circle") => "\u{25E6}".into(),          /* "◦" */
                atom!("square") => "\u{25AA}".into(),          /* "▪" */
                atom!("disclosure-open") => "\u{25BE}".into(), /* "▾" */
                // TODO: Use U+25C2 "◂" depending on the direction.
                atom!("disclosure-closed") => "\u{25B8}".into(), /* "▸" */
                atom!("decimal-leading-zero") if (-9..=9).contains(&value) => {
                    let sign = if value < 0 { "-" } else { "" };
                    format!("{sign}0{}", value.unsigned_abs())
                },
                atom!("lower-alpha") | atom!("lower-latin") => {
                    alphabetic(value, &('a'..='z').collect::<Vec<_>>())
                },
                atom!("upper-alpha") | atom!("upper-latin") => {
                    alphabetic(value, &('A'..='Z').collect::<Vec<_>>())
                },
                atom!("lower-greek") => alphabetic(
                    value,
                    &"αβγδεζηθικλμνξοπρστυφχψω".chars().collect::<Vec<_>>(),
                ),
                atom!("lower-roman") => roman(value).to_lowercase(),
                atom!("upper-roman") => roman(value),
                atom!("arabic-indic") => digits('\u{660}'),
                atom!("bengali") => digits('\u{9E6}'),
                atom!("cambodian") | atom!("khmer") => digits('\u{17E0}'),
                atom!("devanagari") => digits('\u{966}'),
                atom!("gujarati") => digits('\u{AE6}'),
                atom!("gurmukhi") => digits('\u{A66}'),
                atom!("kannada") => digits('\u{CE6}'),
                atom!("lao") => digits('\u{ED0}'),
                atom!("malayalam") => digits('\u{D66}'),
                atom!("mongolian") => digits('\u{1810}'),
                atom!("myanmar") => digits('\u{1040}'),
                atom!("oriya") => digits('\u{B66}'),
                atom!("persian") => digits('\u{6F0}'),
                atom!("tamil") => digits('\u{BE6}'),
                atom!("telugu") => digits('\u{C66}'),
                atom!("thai") => digits('\u{E50}'),
                atom!("tibetan") => digits('\u{F20}'),
                atom!("cjk-decimal") => {
                    numeric(value, &"〇一二三四五六七八九".chars().collect::<Vec<_>>())
                },
                _ => value.to_string(),
            }
        },
        CounterStyle::Symbols { ty, symbols } => {
            let symbols: Vec<&str> = symbols.0.iter().map(symbol_to_string).collect();
            let n = symbols.len() as i64;
            let value_64 = value as i64;
            match ty {
                SymbolsType::Numeric if n >= 2 => numeric(value, &symbols),
                SymbolsType::Cyclic if n >= 1 => {
                    symbols[(value_64 - 1).rem_euclid(n) as usize].to_owned()
                },
                SymbolsType::Alphabetic if n >= 2 => alphabetic(value, &symbols),
                SymbolsType::Symbolic if n >= 1 && value >= 1 => symbols
                    [((value_64 - 1) % n) as usize]
                    .repeat(((value_64 - 1) / n + 1).min(60) as usize),
                SymbolsType::Fixed if value >= 1 && value_64 <= n => {
                    symbols[(value - 1) as usize].to_owned()
                },
                _ => value.to_string(),
            }
        },
    }
}

fn decimal_digits(zero: char) -> Vec<char> {
    (0..10)
        .map(|digit| char::from_u32(zero as u32 + digit).unwrap_or('0'))
        .collect()
}

/// <https://drafts.csswg.org/css-counter-styles-3/#numeric-system>
fn numeric(value: i32, symbols: &[impl ToString]) -> String {
    let base = symbols.len() as u64;
    let mut magnitude = value.unsigned_abs() as u64;
    let mut digits = vec![];
    loop {
        digits.push(symbols[(magnitude % base) as usize].to_string());
        magnitude /= base;
        if magnitude == 0 {
            break;
        }
    }
    digits.reverse();
    let sign = if value < 0 { "-" } else { "" };
    format!("{sign}{}", digits.concat())
}

/// <https://drafts.csswg.org/css-counter-styles-3/#alphabetic-system>
fn alphabetic(value: i32, symbols: &[impl ToString]) -> String {
    if value < 1 {
        return value.to_string();
    }
    let base = symbols.len() as u64;
    let mut magnitude = value as u64;
    let mut letters = vec![];
    while magnitude > 0 {
        magnitude -= 1;
        letters.push(symbols[(magnitude % base) as usize].to_string());
        magnitude /= base;
    }
    letters.reverse();
    letters.concat()
}

/// <https://drafts.csswg.org/css-counter-styles-3/#upper-roman>, defined for 1 to 3999.
fn roman(value: i32) -> String {
    if !(1..=3999).contains(&value) {
        return value.to_string();
    }
    const NUMERALS: [(i32, &str); 13] = [
        (1000, "M"),
        (900, "CM"),
        (500, "D"),
        (400, "CD"),
        (100, "C"),
        (90, "XC"),
        (50, "L"),
        (40, "XL"),
        (10, "X"),
        (9, "IX"),
        (5, "V"),
        (4, "IV"),
        (1, "I"),
    ];
    let mut remaining = value;
    let mut result = String::new();
    for (amount, numeral) in NUMERALS {
        while remaining >= amount {
            result.push_str(numeral);
            remaining -= amount;
        }
    }
    result
}

/// <https://drafts.csswg.org/css-lists/#marker-string>
pub(crate) fn marker_string(list_style_type: &ListStyleType, value: i32) -> Option<String> {
    let suffix = match &list_style_type.0 {
        CounterStyle::None => return None,
        CounterStyle::String(string) => return Some(string.to_string()),
        CounterStyle::Name(name) => match name.0 {
            atom!("disc") |
            atom!("circle") |
            atom!("square") |
            atom!("disclosure-open") |
            atom!("disclosure-closed") => " ",
            atom!("hiragana") |
            atom!("hiragana-iroha") |
            atom!("katakana") |
            atom!("katakana-iroha") |
            atom!("cjk-decimal") |
            atom!("cjk-earthly-branch") |
            atom!("cjk-heavenly-stem") |
            atom!("japanese-informal") |
            atom!("japanese-formal") |
            atom!("simp-chinese-informal") |
            atom!("simp-chinese-formal") |
            atom!("trad-chinese-informal") |
            atom!("trad-chinese-formal") |
            atom!("cjk-ideographic") => "\u{3001}", /* "、" */
            atom!("korean-hangul-formal") |
            atom!("korean-hanja-informal") |
            atom!("korean-hanja-formal") => ", ",
            atom!("ethiopic-numeric") => "/ ",
            _ => ". ",
        },
        CounterStyle::Symbols { .. } => " ",
    };
    Some(format!(
        "{}{}",
        generate_counter_representation(&list_style_type.0, value),
        suffix
    ))
}
