/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Find in page, for the embedder's [`FindInPageRequest`]s.
//!
//! The search runs over the rendered text of the document's flat tree: text nodes that
//! produced a text run in layout (so `display: none` text never matches) and are
//! `visibility: visible`. White space is collapsed like in rendering, letter case is folded
//! unless the search is case sensitive, and the text of one inline formatting context is
//! searched as one string, so a match can span `<b>`, `<span>` and other inline elements but
//! never two paragraphs. Text inside form controls (user agent widgets) is skipped.
//!
//! Matches are painted by layout as highlights of text runs (see
//! [`layout_api::FindHighlight`]), so the page's DOM is never changed.

use embedder_traits::{FindInPageOptions, FindInPageRequest, FindInPageResult};
use js::context::{JSContext, NoGC};
use layout_api::{FindHighlight, QueryMsg, ScrollContainerQueryFlags};
use rustc_hash::FxHashMap;
use script_bindings::codegen::GenericBindings::WindowBinding::ScrollBehavior;
use script_bindings::dom::UnrootedDom;
use servo_base::text::Utf32CodeUnits;

use crate::dom::bindings::codegen::Bindings::ElementBinding::ScrollLogicalPosition;
use crate::dom::bindings::codegen::Bindings::NodeBinding::NodeMethods;
use crate::dom::bindings::inheritance::Castable;
use crate::dom::bindings::root::{Dom, DomRoot};
use crate::dom::bindings::trace::NoTrace;
use crate::dom::document::Document;
use crate::dom::iterators::PrePostIteration;
use crate::dom::node::Node;
use crate::dom::types::{HTMLBRElement, Text};
use crate::dom::window::scrolling_box::ScrollAxisState;

/// Separates the text of different inline formatting contexts in the searched string. It
/// never appears in a query, so no match crosses it.
const SEPARATOR: char = '\0';

/// The state of find in page for one [`Document`].
#[derive(JSTraceable, MallocSizeOf)]
#[cfg_attr(crown, crown::unrooted_must_root_lint::must_root)]
pub(crate) struct FindInPage {
    /// The matches of the current search, in tree order.
    matches: Vec<FindMatch>,
    /// The index of the active match in `matches`.
    active: Option<usize>,
    /// Whether the search stopped at [`FindInPageResult::MATCH_LIMIT`] matches.
    limit_reached: bool,
    /// The text nodes that currently have highlights.
    highlighted: Vec<Dom<Text>>,
    /// The highlights of the nodes in `highlighted`, by node address, for layout.
    #[ignore_malloc_size_of = "Small, and only kept while the find bar is open"]
    highlights: NoTrace<FxHashMap<usize, Vec<FindHighlight>>>,
}

/// One match: the parts of the text nodes it covers, in tree order.
#[derive(JSTraceable, MallocSizeOf)]
#[cfg_attr(crown, crown::unrooted_must_root_lint::must_root)]
struct FindMatch {
    segments: Vec<FindSegment>,
}

/// The part of one text node covered by a match, in `char`s of the node's data.
#[derive(JSTraceable, MallocSizeOf)]
#[cfg_attr(crown, crown::unrooted_must_root_lint::must_root)]
struct FindSegment {
    text: Dom<Text>,
    start: u32,
    end: u32,
}

impl Default for FindInPage {
    fn default() -> Self {
        Self {
            matches: Vec::new(),
            active: None,
            limit_reached: false,
            highlighted: Vec::new(),
            highlights: NoTrace(FxHashMap::default()),
        }
    }
}

impl FindInPage {
    /// The highlights to paint over `node`, read by layout when it builds text runs.
    pub(crate) fn highlights_for_layout(&self, node: &Node) -> Vec<FindHighlight> {
        if self.highlights.0.is_empty() {
            return Vec::new();
        }
        self.highlights
            .0
            .get(&node_key(node))
            .cloned()
            .unwrap_or_default()
    }

    fn result(&self) -> FindInPageResult {
        FindInPageResult {
            match_count: self.matches.len(),
            active_match: self.active,
            limit_reached: self.limit_reached,
        }
    }
}

fn node_key(node: &Node) -> usize {
    node as *const Node as usize
}

/// Fold `character` for comparison: lower case unless the search is case sensitive. Only
/// one-to-one mappings are used, so offsets in the folded text match the original text.
fn fold(character: char, case_sensitive: bool) -> char {
    if case_sensitive {
        return character;
    }
    if character.is_ascii() {
        return character.to_ascii_lowercase();
    }
    let mut lower = character.to_lowercase();
    match (lower.next(), lower.next()) {
        (Some(lower), None) => lower,
        _ => character,
    }
}

/// Soft hyphens are invisible unless a line breaks at them, so they are skipped.
fn is_ignored(character: char) -> bool {
    character == '\u{00AD}'
}

/// A query folded and with its white space collapsed, like the searched text.
fn normalize_query(query: &str, case_sensitive: bool) -> Vec<char> {
    let mut normalized: Vec<char> = Vec::with_capacity(query.len());
    for character in query.chars() {
        if character.is_whitespace() {
            if normalized.last() != Some(&' ') {
                normalized.push(' ');
            }
        } else if character != SEPARATOR && !is_ignored(character) {
            normalized.push(fold(character, case_sensitive));
        }
    }
    normalized
}

/// The rendered text of a document, prepared for searching.
struct SearchText<'no_gc> {
    /// The folded, white-space-collapsed text.
    characters: Vec<char>,
    /// For each of `characters`: the index in `nodes` of the text node it came from and its
    /// offset in that node's data, in `char`s.
    positions: Vec<(u32, u32)>,
    nodes: Vec<UnrootedDom<'no_gc, Node>>,
    /// Where in `characters` the anchor position passed to [`SearchText::collect`] is.
    anchor: Option<usize>,
}

impl<'no_gc> SearchText<'no_gc> {
    fn collect(
        document: &Document,
        no_gc: &'no_gc NoGC,
        case_sensitive: bool,
        anchor: Option<(&Node, u32)>,
    ) -> Self {
        let mut text = SearchText {
            characters: Vec::new(),
            positions: Vec::new(),
            nodes: Vec::new(),
            anchor: None,
        };
        let mut last_formatting_context = None;
        // Whether the last character is collapsible white space (or a separator), so that
        // following white space is dropped.
        let mut after_space = true;

        for item in document
            .upcast::<Node>()
            .following_flat_tree_nodes_unrooted(no_gc)
        {
            let PrePostIteration::Enter(node) = item else {
                continue;
            };
            if node.is::<HTMLBRElement>() {
                // A line break reads as white space between the text around it.
                if !after_space && let Some(&position) = text.positions.last() {
                    text.characters.push(' ');
                    text.positions.push(position);
                    after_space = true;
                }
                continue;
            }
            if !node.is::<Text>() || node.is_in_ua_widget() {
                continue;
            }
            let Some(info) = node
                .layout_data()
                .borrow()
                .as_ref()
                .and_then(|layout_data| layout_data.text_run_find_info())
            else {
                continue;
            };
            if !info.visible {
                continue;
            }
            if last_formatting_context != Some(info.inline_formatting_context) {
                if last_formatting_context.is_some() {
                    text.characters.push(SEPARATOR);
                    text.positions.push((0, 0));
                }
                last_formatting_context = Some(info.inline_formatting_context);
                after_space = true;
            }

            let node_index = text.nodes.len() as u32;
            let is_anchor = anchor.is_some_and(|(anchor, _)| node == *anchor);
            {
                let character_data = node.downcast::<crate::dom::types::CharacterData>();
                let Some(character_data) = character_data else {
                    continue;
                };
                let data = character_data.data();
                for (offset, character) in data.chars().enumerate() {
                    let offset = offset as u32;
                    if is_anchor &&
                        text.anchor.is_none() &&
                        anchor.is_some_and(|(_, anchor_offset)| offset >= anchor_offset)
                    {
                        text.anchor = Some(text.characters.len());
                    }
                    if is_ignored(character) {
                        continue;
                    }
                    if character.is_whitespace() {
                        if after_space {
                            continue;
                        }
                        text.characters.push(' ');
                        after_space = true;
                    } else {
                        text.characters.push(fold(character, case_sensitive));
                        after_space = false;
                    }
                    text.positions.push((node_index, offset));
                }
            }
            if is_anchor && text.anchor.is_none() {
                text.anchor = Some(text.characters.len());
            }
            text.nodes.push(node);
        }
        text
    }

    /// The start of each match of `query`, without overlaps, and whether there were more
    /// than [`FindInPageResult::MATCH_LIMIT`].
    fn find(&self, query: &[char]) -> (Vec<usize>, bool) {
        let mut starts = Vec::new();
        let Some(&first) = query.first() else {
            return (starts, false);
        };
        let haystack = &self.characters;
        let mut index = 0;
        while index + query.len() <= haystack.len() {
            if haystack[index] == first && haystack[index..index + query.len()] == *query {
                if starts.len() == FindInPageResult::MATCH_LIMIT {
                    return (starts, true);
                }
                starts.push(index);
                index += query.len();
            } else {
                index += 1;
            }
        }
        (starts, false)
    }

    /// The text node parts covered by the `length` characters at `start`.
    fn segments(&self, start: usize, length: usize) -> Vec<FindSegment> {
        let mut segments: Vec<FindSegment> = Vec::new();
        let mut last_node = None;
        for &(node_index, offset) in &self.positions[start..start + length] {
            if last_node == Some(node_index) &&
                let Some(segment) = segments.last_mut()
            {
                segment.start = segment.start.min(offset);
                segment.end = segment.end.max(offset + 1);
                continue;
            }
            let Some(text) = self.nodes[node_index as usize].downcast::<Text>() else {
                continue;
            };
            last_node = Some(node_index);
            segments.push(FindSegment {
                text: Dom::from_ref(text),
                start: offset,
                end: offset + 1,
            });
        }
        segments
    }
}

impl Document {
    /// Handle a find-in-page request from the embedder.
    pub(crate) fn handle_find_in_page(
        &self,
        cx: &mut JSContext,
        request: FindInPageRequest,
    ) -> FindInPageResult {
        match request {
            FindInPageRequest::Find { query, options } => self.find_in_page(cx, &query, options),
            FindInPageRequest::Next => self.move_active_find_match(cx, true),
            FindInPageRequest::Previous => self.move_active_find_match(cx, false),
            FindInPageRequest::Stop => {
                let mut state = self.find_in_page_state().borrow_mut();
                state.matches.clear();
                state.active = None;
                state.limit_reached = false;
                self.update_find_highlights(&mut state);
                FindInPageResult::default()
            },
        }
    }

    fn find_in_page(
        &self,
        cx: &mut JSContext,
        query: &str,
        options: FindInPageOptions,
    ) -> FindInPageResult {
        let query = normalize_query(query, options.case_sensitive);

        // Stay near the active match, so that typing more of a word doesn't jump away.
        let anchor = {
            let state = self.find_in_page_state().borrow();
            state
                .active
                .and_then(|active| state.matches.get(active))
                .and_then(|active| active.segments.first())
                .map(|segment| (DomRoot::from_ref(&*segment.text), segment.start))
        };

        // Make sure the text runs reflect the current DOM and style.
        self.window().layout_reflow(cx, QueryMsg::BoxAreas);

        let (matches, active, limit_reached) = {
            let text = SearchText::collect(
                self,
                cx.no_gc(),
                options.case_sensitive,
                anchor
                    .as_ref()
                    .map(|(text, offset)| (text.upcast::<Node>(), *offset)),
            );
            let (starts, limit_reached) = text.find(&query);
            let active = match text.anchor {
                Some(anchor) => starts
                    .iter()
                    .position(|start| *start >= anchor)
                    .or((!starts.is_empty()).then_some(0)),
                None => (!starts.is_empty()).then_some(0),
            };
            let matches: Vec<FindMatch> = starts
                .iter()
                .map(|start| FindMatch {
                    segments: text.segments(*start, query.len()),
                })
                .collect();
            (matches, active, limit_reached)
        };
        // A new search starts at the first match in view, like other browsers.
        let active = match anchor {
            None if !matches.is_empty() => self.first_find_match_in_view(&matches).or(active),
            _ => active,
        };

        {
            let mut state = self.find_in_page_state().borrow_mut();
            state.matches = matches;
            state.active = active;
            state.limit_reached = limit_reached;
            self.update_find_highlights(&mut state);
        }
        self.scroll_active_find_match_into_view(cx);
        self.find_in_page_state().borrow().result()
    }

    fn move_active_find_match(&self, cx: &mut JSContext, forward: bool) -> FindInPageResult {
        {
            let mut state = self.find_in_page_state().borrow_mut();
            let count = state.matches.len();
            if count == 0 {
                return state.result();
            }
            state.active = Some(match (state.active, forward) {
                (None, true) => 0,
                (None, false) => count - 1,
                (Some(active), true) => (active + 1) % count,
                (Some(active), false) => (active + count - 1) % count,
            });
            self.update_find_highlights(&mut state);
        }
        self.scroll_active_find_match_into_view(cx);
        self.find_in_page_state().borrow().result()
    }

    /// The first of `matches` (in tree order) that isn't above the top of the viewport, found
    /// with a binary search since tree order mostly follows the page from top to bottom.
    /// Layout must be up to date.
    fn first_find_match_in_view(&self, matches: &[FindMatch]) -> Option<usize> {
        let layout = self.window().layout();
        let is_above_viewport = |find_match: &FindMatch| {
            find_match.segments.first().is_some_and(|segment| {
                layout
                    .query_text_range_rect(
                        segment.text.upcast::<Node>().to_trusted_node_address(),
                        Utf32CodeUnits(segment.start)..Utf32CodeUnits(segment.end),
                    )
                    .is_some_and(|rect| rect.max_y() <= app_units::Au(0))
            })
        };
        let first = matches.partition_point(is_above_viewport);
        (first < matches.len()).then_some(first)
    }

    /// Hand the highlights of `state`'s matches to layout, replacing the previous ones.
    fn update_find_highlights(&self, state: &mut FindInPage) {
        let FindInPage {
            matches,
            active,
            highlighted,
            highlights,
            ..
        } = state;

        let highlights = &mut highlights.0;
        let previously_highlighted = std::mem::take(highlighted);
        highlights.clear();
        for (index, find_match) in matches.iter().enumerate() {
            for segment in &find_match.segments {
                highlights
                    .entry(node_key(segment.text.upcast()))
                    .or_insert_with(|| {
                        highlighted.push(segment.text.clone());
                        Vec::new()
                    })
                    .push(FindHighlight {
                        start: Utf32CodeUnits(segment.start),
                        end: Utf32CodeUnits(segment.end),
                        active: *active == Some(index),
                    });
            }
        }

        let set = |text: &Text, new_highlights: Vec<FindHighlight>| {
            if let Some(layout_data) = text.upcast::<Node>().layout_data().borrow().as_ref() {
                layout_data.set_text_run_find_highlights(new_highlights);
            }
        };
        for text in &previously_highlighted {
            if !highlights.contains_key(&node_key(text.upcast())) {
                set(text, Vec::new());
            }
        }
        for text in highlighted.iter() {
            let key = node_key(text.upcast());
            set(text, highlights.get(&key).cloned().unwrap_or_default());
        }
        self.window().layout().set_needs_new_display_list();
    }

    /// Scroll the active match into view: not at all if it is already fully visible, else
    /// to the middle of each scroll container.
    fn scroll_active_find_match_into_view(&self, cx: &mut JSContext) {
        let target = {
            let state = self.find_in_page_state().borrow();
            state
                .active
                .and_then(|active| state.matches.get(active))
                .and_then(|active| active.segments.first())
                .map(|segment| {
                    (
                        DomRoot::from_ref(&*segment.text),
                        Utf32CodeUnits(segment.start)..Utf32CodeUnits(segment.end),
                    )
                })
        };
        let Some((text, range)) = target else {
            return;
        };
        let Some(parent) = text.upcast::<Node>().GetParentElement() else {
            return;
        };

        let window = self.window();
        let nearest = ScrollAxisState::new_always_scroll_position(ScrollLogicalPosition::Nearest);
        let center = ScrollAxisState::new_always_scroll_position(ScrollLogicalPosition::Center);
        let mut scrolling_box = parent.scrolling_box(ScrollContainerQueryFlags::empty());
        while let Some(current_box) = scrolling_box {
            scrolling_box = current_box.parent();

            window.layout_reflow(cx, QueryMsg::BoxAreas);
            let Some(rect) = window.layout().query_text_range_rect(
                text.upcast::<Node>().to_trusted_node_address(),
                range.clone(),
            ) else {
                return;
            };
            let current = current_box.scroll_position(cx);
            let mut position =
                current_box.determine_scroll_into_view_position(cx, nearest, nearest, rect);
            if position.y != current.y {
                position =
                    current_box.determine_scroll_into_view_position(cx, center, nearest, rect);
            }
            if position != current {
                current_box.scroll_to(cx, position, ScrollBehavior::Instant);
            }
        }
    }
}
