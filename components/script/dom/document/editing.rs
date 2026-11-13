/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use embedder_traits::{
    ClipboardAction, EditingAction, EditingDirection, EditingMotion, EmbedderMsg, InputEventResult,
    ModifySelection, MouseButton,
};
use js::context::{JSContext, NoGC};
use keyboard_types::{
    CompositionEvent, CompositionState, Key, KeyState, KeyboardEvent as KeyboardTypesEvent,
    Modifiers, NamedKey, ShortcutMatcher,
};
use layout_api::{HitTestFlags, QueryMsg, SegmentGranularity};
use script_bindings::codegen::GenericBindings::DocumentBinding::DocumentMethods;
use script_bindings::codegen::GenericBindings::EventBinding::EventMethods;
use script_bindings::codegen::GenericBindings::MouseEventBinding::MouseEventMethods;
use script_bindings::codegen::GenericBindings::NodeBinding::NodeMethods;
use script_bindings::codegen::GenericBindings::SelectionBinding::SelectionMethods;
use script_bindings::codegen::GenericBindings::UIEventBinding::UIEventMethods as _;
use script_bindings::codegen::GenericBindings::WindowBinding::WindowMethods;
use script_bindings::dom::UnrootedDom;
use script_bindings::inheritance::Castable;
use script_bindings::root::DomRoot;
use script_bindings::str::DOMString;
use servo_base::generic_channel::GenericCallback;
use servo_base::text::RangeAny;
use xml5ever::local_name;

use crate::dom::clipboard::clipboard::may_write_clipboard;
use crate::dom::clipboardevent::ClipboardEventType;
use crate::dom::event::{EventBubbles, EventCancelable};
use crate::dom::execcommand::execcommands::DocumentExecCommandSupport;
use crate::dom::inputevent::HitTestResult;
use crate::dom::selection_range::{RootedSelectionBoundary, SelectionRange};
use crate::dom::text_control::TextControlElement;
use crate::dom::text_input::InputEventType;
use crate::dom::types::{
    ClipboardEvent, DataTransfer, Element, Event, EventTarget, HTMLButtonElement,
    HTMLImageElement, HTMLInputElement, HTMLSelectElement, HTMLTextAreaElement, MouseEvent, Text,
    UIEvent,
};
use crate::dom::iterators::ShadowIncluding;
use crate::dom::{CharacterData, Document, Node, NodeTraits};
use crate::drag::document_selection_drag::{
    DocumentDragSegment, DocumentSelectionDragHandler, adjust_anchor_for_user_select,
};
use crate::drag::drag_data_store::{DragDataStore, Kind, Mode};
use crate::drag::drag_gesture::{DragGesture, DragHandler};

#[cfg(target_os = "macos")]
pub(crate) const CMD_OR_CONTROL: Modifiers = Modifiers::META;
#[cfg(not(target_os = "macos"))]
pub(crate) const CMD_OR_CONTROL: Modifiers = Modifiers::CONTROL;

#[cfg(target_os = "macos")]
pub(crate) const ALT_OR_CONTROL: Modifiers = Modifiers::ALT;
#[cfg(not(target_os = "macos"))]
pub(crate) const ALT_OR_CONTROL: Modifiers = Modifiers::CONTROL;

/// A selection action to use when handling mouse button events.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum MouseButtonSelectionAction {
    /// Select a single selection focus node position. This is created via single primary click
    /// or middle clicks.
    MoveCursor,
    /// Select a range at the target point. This is created via double and triple primary clicks.
    SelectRange(SegmentGranularity),
}

impl MouseButtonSelectionAction {
    pub(crate) fn from_mouse_event(mouse_event: &MouseEvent) -> Option<Self> {
        match mouse_event.button() {
            MouseButton::Primary => {}, // continue below
            MouseButton::Auxiliary => return Some(Self::MoveCursor),
            _ => return None,
        }

        // We currently don't do anything for higher click counts, but some platforms do.
        // For example on desktop linux, 4+ click cycle back starting at same as single click:
        // ```
        // const MAX_CLICKS: i32 = 3;
        // let click_count = (click_count - 1) % MAX_CLICKS + 1;
        // ```
        // We should re-examine this when implementing support for platform-specific editing
        // behaviors.
        let click_count = mouse_event.upcast::<UIEvent>().Detail();
        match click_count {
            1 => Some(Self::MoveCursor),
            2 => Some(Self::SelectRange(SegmentGranularity::Word)),
            3 => Some(Self::SelectRange(SegmentGranularity::HardLineBreak)),
            _ => None,
        }
    }
}

impl Document {
    pub(crate) fn editing_context(&self, no_gc: &NoGC, node: &Node) -> EditingContext {
        if let Ok(editing_context) = EditingContext::try_from(node) {
            return editing_context;
        }

        let mut current_node = UnrootedDom::from_ref(node, no_gc);
        while let Some(parent) = current_node.parent_in_flat_tree(no_gc).into_parent() {
            if let Ok(editing_context) = EditingContext::try_from(&**parent) {
                return editing_context;
            }
            current_node = parent;
        }

        EditingContext::Document(DomRoot::from_ref(self))
    }

    /// Stillwater: `document.execCommand('copy')` and `execCommand('cut')`, the commands
    /// "Copy" buttons use to copy a selected (often hidden) text field or a selected range,
    /// often with a `copy` handler that sets its own data. They run the same clipboard action
    /// as Ctrl+C / Ctrl+X on the focused element (the text field `select()` focused, else the
    /// document's selection), but only while the page has transient activation
    /// (see [`may_write_clipboard`]); otherwise they do nothing and return false.
    /// `execCommand('paste')` stays unsupported: pages can't read the clipboard.
    pub(crate) fn exec_clipboard_command(
        &self,
        cx: &mut JSContext,
        action: ClipboardAction,
    ) -> bool {
        thread_local! {
            /// A `copy` handler calling `execCommand('copy')` again does nothing.
            static RUNNING: Cell<bool> = const { Cell::new(false) };
        }
        if RUNNING.get() || !may_write_clipboard(self.window()) {
            return false;
        }
        RUNNING.set(true);
        let focused_node = self.focus_handler().focused_area().dom_anchor(self);
        let editing_context = self.editing_context(cx.no_gc(), &focused_node);
        self.handle_clipboard_action(cx, &editing_context, action);
        RUNNING.set(false);
        true
    }

    /// <https://www.w3.org/TR/clipboard-apis/#clipboard-actions>
    pub(crate) fn handle_clipboard_action(
        &self,
        cx: &mut JSContext,
        editing_context: &EditingContext,
        action: ClipboardAction,
    ) -> InputEventResult {
        let clipboard_event_type = match action {
            ClipboardAction::Copy => ClipboardEventType::Copy,
            ClipboardAction::Cut => ClipboardEventType::Cut,
            ClipboardAction::Paste => ClipboardEventType::Paste,
        };

        // The script_triggered flag is set if the action runs because of a script, e.g. document.execCommand()
        let script_triggered = false;

        // The script_may_access_clipboard flag is set
        // if action is paste and the script thread is allowed to read from clipboard or
        // if action is copy or cut and the script thread is allowed to modify the clipboard
        let script_may_access_clipboard = false;

        // Step 1. If the script-triggered flag is set and the script-may-access-clipboard flag is unset
        if script_triggered && !script_may_access_clipboard {
            return InputEventResult::empty();
        }

        // Step 2. Fire a clipboard event
        let event_target = editing_context.event_target();
        let clipboard_event = self.fire_clipboard_event(cx, &event_target, clipboard_event_type);

        let event = clipboard_event.upcast::<Event>();
        if !event.DefaultPrevented() {
            // Step 3. If the event was not canceled, then
            match clipboard_event.clipboard_event_type() {
                ClipboardEventType::Copy if editing_context.copying_enabled() => {
                    // Step 3.1. Copy the selected contents, if any, to the clipboard.
                    // Implementations should create alternate text/html and text/plain
                    // clipboard formats when content in a web page is selected.
                    if let Some(selection) = editing_context.selection_content(cx) {
                        match editing_context.selection_html(cx) {
                            Some(html) => self.send_to_embedder(EmbedderMsg::SetClipboardHtml(
                                self.webview_id(),
                                selection,
                                html,
                            )),
                            None => self.send_to_embedder(EmbedderMsg::SetClipboardText(
                                self.webview_id(),
                                selection,
                            )),
                        }
                    }
                    // Step 3.2. Fire a clipboard event named clipboardchange
                    self.fire_clipboard_event(cx, &event_target, ClipboardEventType::Change);

                    // This is how `true` is returned from this function.
                    event.mark_as_handled();
                },
                ClipboardEventType::Cut => {
                    if let Some(selection) = editing_context.selection_content(cx) &&
                        editing_context.cutting_enabled()
                    {
                        let canceled = editing_context.fire_cut_beforeinput_event(cx);
                        if canceled {
                            return InputEventResult::empty();
                        }

                        // Step 3.1. If there is a selection in an editable context where
                        // cutting is enabled, then
                        // Step 3.1.1. Copy the selected contents, if any, to the clipboard.
                        // Implementations should create alternate text/html and text/plain
                        // clipboard formats when content in a web page is selected.
                        self.send_to_embedder(EmbedderMsg::SetClipboardText(
                            self.webview_id(),
                            selection,
                        ));

                        // Step 3.1.2. Remove the contents of the selection from the document
                        // and collapse the selection.
                        editing_context.remove_the_contents_of_the_selection(cx);

                        // Step 3.1.3. Fire a clipboard event named clipboardchange
                        self.fire_clipboard_event(cx, &event_target, ClipboardEventType::Change);

                        // Step 3.1.4. Queue tasks to fire any events that should fire due to
                        // the modification, see §5.3 Integration with other scripts and
                        // events for details.
                        editing_context.fire_cut_events();

                        // This is how `true` is returned from this function.
                        event.mark_as_handled();
                    }
                },
                ClipboardEventType::Paste => {
                    if editing_context.has_selection_or_cursor() &&
                        editing_context.pasting_enabled() &&
                        let Some(text_content) = clipboard_event.text_content()
                    {
                        let canceled =
                            editing_context.fire_paste_beforeinput_event(cx, &text_content);
                        if canceled {
                            return InputEventResult::empty();
                        }

                        // Step 3.1. If there is a selection or cursor in an editable context
                        // where pasting is enabled, then
                        // Step 3.1.1. Insert the most suitable content found on the
                        // clipboard, if any, into the context.
                        editing_context.insert_content(cx, &text_content);

                        // Step 3.1.2. Queue tasks to fire any events that should fire due to
                        // the modification, see §5.3 Integration with other scripts and
                        // events for details.
                        editing_context.fire_paste_events(&text_content);

                        // This is how `true` is returned from this function.
                        event.mark_as_handled();
                    }
                },
                _ => (),
            }
        } else {
            // Step 4 If the event was canceled, then
            match clipboard_event.clipboard_event_type() {
                ClipboardEventType::Copy => {
                    // Step 4.1 Call the write content to the clipboard algorithm,
                    // passing on the DataTransferItemList items, a clear-was-called flag and a types-to-clear list.
                    if let Some(clipboard_data) = clipboard_event.clipboard_data() {
                        let drag_data_store =
                            clipboard_data.data_store().expect("This shouldn't fail");
                        self.write_content_to_the_clipboard(&drag_data_store);
                    }
                },
                ClipboardEventType::Cut => {
                    // Step 4.1 Call the write content to the clipboard algorithm,
                    // passing on the DataTransferItemList items, a clear-was-called flag and a types-to-clear list.
                    if let Some(clipboard_data) = clipboard_event.clipboard_data() {
                        let drag_data_store =
                            clipboard_data.data_store().expect("This shouldn't fail");
                        self.write_content_to_the_clipboard(&drag_data_store);
                    }

                    // Step 4.2 Fire a clipboard event named clipboardchange
                    self.fire_clipboard_event(cx, &event_target, ClipboardEventType::Change);
                },
                // Step 4.1 Return false.
                ClipboardEventType::Paste => (),
                _ => (),
            }
        }

        // Step 5: Return true from the action.
        // In this case we are returning the `InputEventResult` instead of true or false.
        event.flags().into()
    }

    /// <https://www.w3.org/TR/clipboard-apis/#fire-a-clipboard-event>
    fn fire_clipboard_event(
        &self,
        cx: &mut JSContext,
        target: &EventTarget,
        clipboard_event_type: ClipboardEventType,
    ) -> DomRoot<ClipboardEvent> {
        let clipboard_event = ClipboardEvent::new(
            cx,
            self.window(),
            None,
            clipboard_event_type,
            EventBubbles::Bubbles,
            EventCancelable::Cancelable,
            None,
        );

        // Step 1 Let clear_was_called be false
        // Step 2 Let types_to_clear an empty list
        let mut drag_data_store = DragDataStore::new();

        // Step 4 let clipboard-entry be the sequence number of clipboard content, null if the OS doesn't support it.

        // Step 5 let trusted be true if the event is generated by the user agent, false otherwise
        let trusted = true;

        // Step 6 if the context is editable:
        // Step 6.2 else TODO require Selection see https://github.com/w3c/clipboard-apis/issues/70
        // Step 7
        match clipboard_event.clipboard_event_type() {
            ClipboardEventType::Copy | ClipboardEventType::Cut => {
                // Step 7.2.1
                drag_data_store.set_mode(Mode::ReadWrite);
            },
            ClipboardEventType::Paste => {
                let (callback, receiver) =
                    GenericCallback::new_blocking().expect("Could not create callback");
                self.send_to_embedder(EmbedderMsg::GetClipboardText(self.webview_id(), callback));
                let text_contents = receiver
                    .recv()
                    .map(Result::unwrap_or_default)
                    .unwrap_or_default();

                // Step 7.1.1
                drag_data_store.set_mode(Mode::ReadOnly);
                // Step 7.1.2 If trusted or the implementation gives script-generated events access to the clipboard
                if trusted {
                    // Step 7.1.2.1 For each clipboard-part on the OS clipboard:

                    // Step 7.1.2.1.1 If clipboard-part contains plain text, then
                    let data = DOMString::from(text_contents);
                    let type_ = DOMString::from_static("text/plain");
                    let _ = drag_data_store.add(Kind::Text { data, type_ });

                    // Step 7.1.2.1.2 TODO If clipboard-part represents file references, then for each file reference
                    // Step 7.1.2.1.3 TODO If clipboard-part contains HTML- or XHTML-formatted text then

                    // Step 7.1.3 Update clipboard-event-data’s files to match clipboard-event-data’s items
                    // Step 7.1.4 Update clipboard-event-data’s types to match clipboard-event-data’s items
                }
            },
            ClipboardEventType::Change | ClipboardEventType::Other(..) => (),
        }

        // Step 3
        let clipboard_event_data = DataTransfer::new(
            cx,
            self.window(),
            Rc::new(RefCell::new(Some(drag_data_store))),
        );

        // Step 8
        clipboard_event.set_clipboard_data(Some(&clipboard_event_data));

        // Step 9
        let event = clipboard_event.upcast::<Event>();
        event.set_trusted(trusted);

        // Step 10 Set event’s composed to true.
        event.set_composed(true);

        // Step 11
        event.dispatch(cx, target, false);

        DomRoot::from(clipboard_event)
    }

    /// <https://www.w3.org/TR/clipboard-apis/#write-content-to-the-clipboard>
    fn write_content_to_the_clipboard(&self, drag_data_store: &DragDataStore) {
        // Step 1
        if drag_data_store.list_len() > 0 {
            // Step 1.1 Clear the clipboard.
            self.send_to_embedder(EmbedderMsg::ClearClipboard(self.webview_id()));
            // Step 1.2
            // Stillwater: a page that sets both `text/plain` and `text/html` gets both on
            // the clipboard (before, each part replaced the previous one as plain text, so
            // the HTML source could end up as the pasted text).
            let mut plain_text = None;
            let mut html = None;
            for item in drag_data_store.iter_item_list() {
                match item {
                    Kind::Text { data, type_ } => {
                        // Step 1.2.1.1 Ensure encoding is correct per OS and locale conventions
                        // Step 1.2.1.2 Normalize line endings according to platform conventions
                        // Step 1.2.1.3
                        if type_.to_string().eq_ignore_ascii_case("text/html") {
                            html = Some(data.to_string());
                        } else if plain_text.is_none() ||
                            type_.to_string().eq_ignore_ascii_case("text/plain")
                        {
                            plain_text = Some(data.to_string());
                        }
                    },
                    Kind::File { .. } => {
                        // Step 1.2.2 If data is of a type listed in the mandatory data types list, then
                        // Step 1.2.2.1 Place part on clipboard with the appropriate OS clipboard format description
                        // Step 1.2.3 Else this is left to the implementation
                    },
                }
            }
            match (plain_text, html) {
                (plain_text, Some(html)) => self.send_to_embedder(EmbedderMsg::SetClipboardHtml(
                    self.webview_id(),
                    plain_text.unwrap_or_default(),
                    html,
                )),
                (Some(plain_text), None) => self.send_to_embedder(
                    EmbedderMsg::SetClipboardText(self.webview_id(), plain_text),
                ),
                (None, None) => {},
            }
        } else {
            // Step 2.1
            if drag_data_store.clear_was_called {
                // Step 2.1.1 If types-to-clear list is empty, clear the clipboard
                self.send_to_embedder(EmbedderMsg::ClearClipboard(self.webview_id()));
                // Step 2.1.2 Else remove the types in the list from the clipboard
                // As of now this can't be done with Arboard, and it's possible that will be removed from the spec
            }
        }
    }

    /// <https://w3c.github.io/editing/docs/execCommand/#additional-requirements>
    ///
    /// This function does not do any checks for whether or not we are actually inside an
    /// editing host, since those checks are performed by exec_command_for_command_id
    /// either way.
    pub(crate) fn perform_editing_action(
        &self,
        cx: &mut JSContext,
        editing_action: &EditingAction,
    ) -> bool {
        if !servo_config::pref!(dom_exec_command_enabled) {
            return false;
        }

        // This function does not do any checks for whether or not we are actually inside an
        // editing host, since those checks are performed by exec_command_for_command_id either way.
        let (command, argument) = match editing_action {
            // TODO: composition should be supported once editing hosts have selection support
            EditingAction::MoveCursor(..) |
            EditingAction::StartComposition |
            EditingAction::EndComposition(..) => return false,
            EditingAction::SelectAll | EditingAction::Clipboard(_) => {
                unreachable!("Should have been handled before this point.")
            },
            EditingAction::InsertNewline => {
                // > When the user instructs the user agent to insert a line break inside an
                // > editing host without breaking out of the current block, such as by
                // > pressing Shift-Enter or Option-Enter while the cursor is in an editable
                // > node, the user agent must call execCommand("insertlinebreak") on the
                // > relevant document.
                (DOMString::from_static("insertlinebreak"), DOMString::new())
            },
            EditingAction::InsertParagraph => {
                // > When the user instructs the user agent to insert a line break inside an
                // > editing host, such as by pressing the Enter key while the cursor is in an
                // > editable node, the user agent must call execCommand("insertparagraph") on
                // > the relevant document.
                (DOMString::from_static("insertparagraph"), DOMString::new())
            },
            // > When the user instructs the user agent to delete the previous character inside an
            // > editing host, such as by pressing the Backspace key while the cursor is in an
            // > editable node, the user agent must call execCommand("delete") on the relevant
            // > document.
            //
            // TODO: Handle other types of motions here.
            EditingAction::Backspace(..) => (DOMString::from_static("delete"), DOMString::new()),
            // > When the user instructs the user agent to delete the next character inside an
            // > editing host, such as by pressing the Delete key while the cursor is in an
            // > editable node, the user agent must call execCommand("forwarddelete") on the
            // > relevant document
            EditingAction::Delete => (DOMString::from_static("forwarddelete"), DOMString::new()),
            // > When the user instructs the user agent to insert text inside an editing host, such
            // > as by typing on the keyboard while the cursor is in an editable node, the user
            // > agent must call execCommand("inserttext", false, value) on the relevant document,
            // > with value equal to the text the user provided. If the user inserts multiple
            // > characters at once or in quick succession, this specification does not define
            // > whether it is treated as one insertion or several consecutive insertions.
            // TODO: composition should be supported once editing hosts have selection support
            EditingAction::InsertText(text) | EditingAction::InsertCompositionText(text) => (
                DOMString::from_static("inserttext"),
                DOMString::from(text.as_str()),
            ),
        };

        self.exec_command_for_command_id(cx, command, argument)
    }

    fn handle_mousedown_event(
        &self,
        cx: &mut JSContext,
        mouse_event: &MouseEvent,
        hit_test_result: &HitTestResult,
    ) {
        assert_eq!(mouse_event.upcast::<Event>().type_(), atom!("mousedown"));

        let Some(selection) = self.GetSelection(cx) else {
            return;
        };

        if mouse_event.button() == MouseButton::Auxiliary {
            let _ = selection.Collapse(cx, None, 0);
            mouse_event.upcast::<Event>().mark_as_handled();
            return;
        }

        if mouse_event.button() != MouseButton::Primary {
            return;
        }

        let Some(action) = MouseButtonSelectionAction::from_mouse_event(mouse_event) else {
            return;
        };

        // Stillwater: like Chrome, a press on an image (it may start a drag) or on a button
        // or a `<select>` leaves the selection as it is: it neither moves the caret there,
        // nor selects a "word" on a double click, nor starts (or extends) a drag selection.
        if press_leaves_selection_alone(&hit_test_result.node) {
            return;
        }

        // When the hit test cannot find a suitable DOM position for selection, just
        // use the first offset within the target node of the `mousedown` event. This
        // is a reasonable place to start the selection from.
        let boundary = hit_test_result
            .dom_position_for_selection
            .clone()
            .unwrap_or_else(|| RootedSelectionBoundary::start_of(&hit_test_result.node));
        let Some((boundary, user_select_contain_node)) =
            adjust_anchor_for_user_select(cx, boundary)
        else {
            return;
        };

        // Stillwater: Shift+click extends the selection from its anchor to the click
        // (Shift+double-click to the edge of the clicked word), as in Chrome and Firefox;
        // a Shift+drag keeps extending it.
        if mouse_event.ShiftKey() &&
            let Some(anchor) = selection.composed_anchor_position()
        {
            let focus = match action {
                MouseButtonSelectionAction::MoveCursor => Some(boundary.clone()),
                MouseButtonSelectionAction::SelectRange(granularity) => self
                    .window()
                    .query_text_segment(
                        cx,
                        &boundary.container,
                        boundary.utf32_offset(),
                        granularity,
                    )
                    .map(|(start, end)| {
                        if boundary.cmp(cx.no_gc(), &anchor) == std::cmp::Ordering::Less {
                            start
                        } else {
                            end
                        }
                    }),
            };
            if let Some(focus) = focus {
                selection.set_anchor_and_focus(cx, &anchor, &focus);
                self.event_handler().install_drag_gesture(DragGesture::new(
                    DragHandler::DocumentSelection(DocumentSelectionDragHandler::new(
                        self,
                        user_select_contain_node.as_deref(),
                        None,
                    )),
                ));
                mouse_event.upcast::<Event>().mark_as_handled();
                return;
            }
        }

        // Stillwater: <https://w3c.github.io/selection-api/#selectstart-event>: a cancelable
        // `selectstart` fires where the selection starts; cancelling it leaves the
        // selection alone (sites use this to make content unselectable).
        if !boundary
            .container
            .upcast::<EventTarget>()
            .fire_bubbling_cancelable_event(cx, atom!("selectstart"))
        {
            return;
        }

        let segment_data = match action {
            MouseButtonSelectionAction::MoveCursor => {
                selection.collapse_to_dom_position(cx, &boundary);
                None
            },
            MouseButtonSelectionAction::SelectRange(granularity) => self
                .window()
                .query_text_segment(
                    cx,
                    &boundary.container,
                    boundary.utf32_offset(),
                    granularity,
                )
                .map(|(start, end)| {
                    selection.set_anchor_and_focus(cx, &start, &end);
                    (granularity, start, end)
                }),
        };

        self.event_handler().install_drag_gesture(DragGesture::new(
            DragHandler::DocumentSelection(DocumentSelectionDragHandler::new(
                self,
                user_select_contain_node.as_deref(),
                segment_data.map(|(granularity, start, end)| DocumentDragSegment {
                    granularity,
                    range: SelectionRange::new_from_rooted(start, end),
                }),
            )),
        ));

        mouse_event.upcast::<Event>().mark_as_handled();
    }
}

/// Stillwater: whether a primary press on `node` leaves the page selection alone, as in
/// Chrome: an image that can be dragged (Chrome only starts a selection on mouse moves that
/// don't drag it, and dragging images isn't supported here yet), and the contents of a
/// button or a `<select>`.
fn press_leaves_selection_alone(node: &Node) -> bool {
    node.inclusive_ancestors(ShadowIncluding::Yes).any(|ancestor| {
        if ancestor.is::<HTMLImageElement>() {
            return ancestor.downcast::<Element>().is_none_or(|element| {
                element.get_string_attribute(&local_name!("draggable")) != "false"
            });
        }
        ancestor.is::<HTMLButtonElement>() || ancestor.is::<HTMLSelectElement>()
    })
}

pub(crate) enum TextControlElementEditingContext {
    TextArea(DomRoot<HTMLTextAreaElement>),
    Input(DomRoot<HTMLInputElement>),
}

impl TextControlElementEditingContext {
    fn text_control_element(&self) -> &dyn TextControlElement {
        match self {
            TextControlElementEditingContext::TextArea(text_area) => &**text_area,
            TextControlElementEditingContext::Input(input) => &**input,
        }
    }

    fn handle_mousedown_event(&self, mouse_event: &MouseEvent, hit_test_result: &HitTestResult) {
        let text_control = self.text_control_element();
        if text_control.text_input_mut().handle_mousedown_event(
            text_control.as_element(),
            mouse_event,
            hit_test_result,
        ) {
            text_control.maybe_update_shared_selection();
            // Stillwater: a double or triple click that selects text fires `select`.
            if text_control.has_uncollapsed_selection() {
                text_control.queue_select_event();
            }
            mouse_event.upcast::<Event>().mark_as_handled();
        }
    }
}

pub(crate) enum EditingContext {
    TextControl(TextControlElementEditingContext),
    Document(DomRoot<Document>),
}

impl TryFrom<&Node> for EditingContext {
    type Error = ();

    fn try_from(node: &Node) -> Result<Self, Self::Error> {
        if let Some(text_area) = node.downcast::<HTMLTextAreaElement>() {
            return Ok(EditingContext::TextControl(
                TextControlElementEditingContext::TextArea(DomRoot::from_ref(text_area)),
            ));
        }
        if let Some(input) = node.downcast::<HTMLInputElement>() &&
            input.is_textual_or_password()
        {
            return Ok(EditingContext::TextControl(
                TextControlElementEditingContext::Input(DomRoot::from_ref(input)),
            ));
        }
        Err(())
    }
}

impl EditingContext {
    pub(crate) fn event_target(&self) -> DomRoot<EventTarget> {
        match self {
            EditingContext::TextControl(element) => match element {
                TextControlElementEditingContext::TextArea(text_area) => {
                    DomRoot::from_ref(text_area.upcast())
                },
                TextControlElementEditingContext::Input(input) => DomRoot::from_ref(input.upcast()),
            },
            EditingContext::Document(document) => {
                document.event_handler().target_for_events_following_focus()
            },
        }
    }

    pub(crate) fn document(&self) -> DomRoot<Document> {
        match self {
            EditingContext::TextControl(element) => match element {
                TextControlElementEditingContext::TextArea(element) => element.owner_document(),
                TextControlElementEditingContext::Input(element) => element.owner_document(),
            },
            EditingContext::Document(document) => document.clone(),
        }
    }

    pub(crate) fn selection_content(&self, cx: &mut JSContext) -> Option<String> {
        match self {
            EditingContext::TextControl(element) => element
                .text_control_element()
                .text_input()
                .selection_content(),
            EditingContext::Document(document) => document
                .selection()
                .map(|selection| selection.Stringifier(cx).to_string())
                .filter(|selection| !selection.is_empty()),
        }
    }

    /// Stillwater: the selection as an HTML fragment for the clipboard's `text/html`
    /// format. Text controls copy plain text only.
    pub(crate) fn selection_html(&self, cx: &mut JSContext) -> Option<String> {
        match self {
            EditingContext::TextControl(..) => None,
            EditingContext::Document(document) => document
                .selection()
                .and_then(|selection| selection.html_for_clipboard(cx)),
        }
    }

    pub(crate) fn has_uncollapsed_selection(&self) -> bool {
        match self {
            EditingContext::TextControl(element) => {
                element.text_control_element().has_uncollapsed_selection()
            },
            EditingContext::Document(document) => document
                .selection()
                .is_some_and(|selection| !selection.collapsed()),
        }
    }

    pub(crate) fn has_selectable_text(&self) -> bool {
        match self {
            EditingContext::TextControl(element) => {
                element.text_control_element().has_selectable_text()
            },
            EditingContext::Document(..) => true,
        }
    }

    pub(crate) fn has_selection_or_cursor(&self) -> bool {
        match self {
            EditingContext::TextControl(..) => true,
            EditingContext::Document(document) => document
                .selection()
                .is_some_and(|selection| selection.RangeCount() > 0),
        }
    }

    pub(crate) fn copying_enabled(&self) -> bool {
        match self {
            EditingContext::TextControl(element) => {
                !element.text_control_element().is_password_field()
            },
            EditingContext::Document(..) => true,
        }
    }

    pub(crate) fn cutting_enabled(&self) -> bool {
        // `pasting_enabled` ensures that the editable content can be modified, which is necessary
        // for cutting text.
        self.copying_enabled() && self.pasting_enabled()
    }

    pub(crate) fn pasting_enabled(&self) -> bool {
        match self {
            EditingContext::TextControl(element) => {
                !element.text_control_element().read_only_or_disabled()
            },
            EditingContext::Document(..) => {
                // TODO(mrobinson): Add support for integration with contenteditable.
                false
            },
        }
    }

    pub(crate) fn remove_the_contents_of_the_selection(&self, cx: &mut JSContext) {
        match self {
            EditingContext::TextControl(element) => {
                element
                    .text_control_element()
                    .remove_the_contents_of_the_selection(cx);
            },
            EditingContext::Document(..) => {
                // TODO(mrobinson): Add support for integration with contenteditable.
            },
        }
    }

    /// Returns true if and only if the cut should be canceled.
    pub(crate) fn fire_cut_beforeinput_event(&self, cx: &mut JSContext) -> bool {
        match self {
            EditingContext::TextControl(element) => element
                .text_control_element()
                .fire_beforeinput_event(cx, None, InputEventType::DeleteByCut),
            EditingContext::Document(..) => false,
        }
    }

    /// Returns true if and only if the paste should be canceled.
    pub(crate) fn fire_paste_beforeinput_event(&self, cx: &mut JSContext, text: &str) -> bool {
        match self {
            EditingContext::TextControl(element) => element
                .text_control_element()
                .fire_beforeinput_event(cx, Some(text), InputEventType::InsertFromPaste),
            EditingContext::Document(..) => false,
        }
    }

    pub(crate) fn fire_cut_events(&self) {
        match self {
            EditingContext::TextControl(element) => {
                element
                    .text_control_element()
                    .queue_input_event(None, InputEventType::DeleteByCut);
            },
            EditingContext::Document(..) => {},
        }
    }

    pub(crate) fn insert_content(&self, cx: &mut JSContext, text_content: &str) {
        match self {
            EditingContext::TextControl(element) => {
                element
                    .text_control_element()
                    .insert_content(cx, text_content);
            },
            EditingContext::Document(..) => {
                // TODO(mrobinson): Add support for integration with contenteditable.
            },
        }
    }

    pub(crate) fn fire_paste_events(&self, text_content: &str) {
        match self {
            EditingContext::TextControl(element) => {
                element.text_control_element().queue_input_event(
                    Some(text_content.to_owned()),
                    InputEventType::InsertFromPaste,
                );
            },
            EditingContext::Document(..) => {},
        }
    }

    pub(crate) fn select_all(&self, cx: &mut JSContext) {
        match self {
            EditingContext::TextControl(element) => element.text_control_element().select_all(),
            EditingContext::Document(document) => {
                let Some(selection) = document.GetSelection(cx) else {
                    return;
                };
                if let Some(node) = document
                    .GetBody()
                    .map(DomRoot::upcast::<Node>)
                    .or_else(|| document.GetDocumentElement().map(DomRoot::upcast::<Node>))
                {
                    let _ = selection.SelectAllChildren(cx, &node);
                }
            },
        }
    }

    pub(crate) fn perform_editing_action(&self, cx: &mut JSContext, action: EditingAction) -> bool {
        if let Some(node) = self.event_target().downcast::<Node>() {
            // A previous event listener, such as keydown or beforeinput, might have
            // hidden the event target. So we should re-check the layout and see if
            // the node is still being rendered before performing an editing action
            // on it.
            node.owner_window().layout_reflow(cx, QueryMsg::StyleQuery);
            if !node.is_being_rendered_or_delegates_rendering(None) {
                return false;
            }
        }

        // A couple editing actions can be performed via the external interface of
        // `EditingContext`. If that's the case we want to do that so that the same
        // code path is used regardless of how the action was executed.
        if let EditingAction::Clipboard(clipboard_action) = action {
            return self
                .document()
                .handle_clipboard_action(cx, self, clipboard_action)
                .contains(InputEventResult::Consumed);
        }

        if let EditingAction::SelectAll = action {
            self.select_all(cx);
            return true;
        }

        if let EditingContext::Document(document) = self &&
            let EditingAction::MoveCursor(direction, motion, ModifySelection::Yes) = action
        {
            return document.extend_selection_with_keyboard(cx, direction, motion);
        }

        match self {
            EditingContext::TextControl(element) => element
                .text_control_element()
                .perform_editing_action(cx, action),
            EditingContext::Document(document) => document.perform_editing_action(cx, &action),
        }
    }

    pub(crate) fn handle_mousedown_event(
        &self,
        cx: &mut JSContext,
        mouse_event: &MouseEvent,
        hit_test_result: &HitTestResult,
    ) {
        match self {
            EditingContext::TextControl(element) => {
                element.handle_mousedown_event(mouse_event, hit_test_result)
            },
            EditingContext::Document(document) => {
                document.handle_mousedown_event(cx, mouse_event, hit_test_result)
            },
        }
    }
}

pub(crate) fn editing_action_from_keyboard_event(
    event: &KeyboardTypesEvent,
) -> Option<EditingAction> {
    let mut modifiers = event.modifiers;
    if let Some(action) = ShortcutMatcher::new(event.state, event.key.clone(), modifiers)
        .shortcut(CMD_OR_CONTROL, 'A', || Some(EditingAction::SelectAll))
        .shortcut(CMD_OR_CONTROL, 'C', || {
            Some(EditingAction::Clipboard(ClipboardAction::Copy))
        })
        .shortcut(CMD_OR_CONTROL, 'X', || {
            Some(EditingAction::Clipboard(ClipboardAction::Cut))
        })
        .shortcut(CMD_OR_CONTROL, 'V', || {
            Some(EditingAction::Clipboard(ClipboardAction::Paste))
        })
        // TODO: Figure out if the bit about Option+Enter works and whether or not this
        //       ends up providing the correct behavior on Mac. (i.e. whether or not
        //       Shift should be accepted in addition to Option.)
        .shortcut(Modifiers::SHIFT, Key::Named(NamedKey::Enter), || {
            Some(EditingAction::InsertNewline)
        })
        .shortcut(Modifiers::empty(), Key::Named(NamedKey::Enter), || {
            Some(EditingAction::InsertParagraph)
        })
        .otherwise(|| None)
        .flatten()
    {
        return Some(action);
    };

    let update_selection = if modifiers.contains(Modifiers::SHIFT) {
        ModifySelection::Yes
    } else {
        ModifySelection::No
    };
    modifiers.remove(Modifiers::SHIFT);

    ShortcutMatcher::new(KeyState::Down, event.key.clone(), modifiers)
        .shortcut(Modifiers::empty(), Key::Named(NamedKey::ArrowLeft), || {
            Some(EditingAction::MoveCursor(
                EditingDirection::Backward,
                EditingMotion::Grapheme,
                update_selection,
            ))
        })
        .shortcut(Modifiers::empty(), Key::Named(NamedKey::ArrowRight), || {
            Some(EditingAction::MoveCursor(
                EditingDirection::Forward,
                EditingMotion::Grapheme,
                update_selection,
            ))
        })
        .shortcut(Modifiers::empty(), Key::Named(NamedKey::ArrowUp), || {
            Some(EditingAction::MoveCursor(
                EditingDirection::Backward,
                EditingMotion::Line,
                update_selection,
            ))
        })
        .shortcut(Modifiers::empty(), Key::Named(NamedKey::ArrowDown), || {
            Some(EditingAction::MoveCursor(
                EditingDirection::Forward,
                EditingMotion::Line,
                update_selection,
            ))
        })
        .shortcut(Modifiers::CONTROL | Modifiers::ALT, 'B', || {
            Some(EditingAction::MoveCursor(
                EditingDirection::Backward,
                EditingMotion::Word,
                update_selection,
            ))
        })
        .shortcut(ALT_OR_CONTROL, Key::Named(NamedKey::ArrowLeft), || {
            Some(EditingAction::MoveCursor(
                EditingDirection::Backward,
                EditingMotion::Word,
                update_selection,
            ))
        })
        .shortcut(Modifiers::CONTROL | Modifiers::ALT, 'F', || {
            Some(EditingAction::MoveCursor(
                EditingDirection::Forward,
                EditingMotion::Word,
                update_selection,
            ))
        })
        .shortcut(ALT_OR_CONTROL, Key::Named(NamedKey::ArrowRight), || {
            Some(EditingAction::MoveCursor(
                EditingDirection::Forward,
                EditingMotion::Word,
                update_selection,
            ))
        })
        .shortcut(Modifiers::empty(), Key::Named(NamedKey::Home), || {
            Some(EditingAction::MoveCursor(
                EditingDirection::Backward,
                EditingMotion::LineStartOrEnd,
                update_selection,
            ))
        })
        .optional_shortcut(cfg!(target_os = "macos"), Modifiers::CONTROL, 'A', || {
            Some(EditingAction::MoveCursor(
                EditingDirection::Backward,
                EditingMotion::LineStartOrEnd,
                update_selection,
            ))
        })
        .shortcut(Modifiers::empty(), Key::Named(NamedKey::End), || {
            Some(EditingAction::MoveCursor(
                EditingDirection::Forward,
                EditingMotion::LineStartOrEnd,
                update_selection,
            ))
        })
        .optional_shortcut(cfg!(target_os = "macos"), Modifiers::CONTROL, 'E', || {
            Some(EditingAction::MoveCursor(
                EditingDirection::Backward,
                EditingMotion::LineStartOrEnd,
                update_selection,
            ))
        })
        .optional_shortcut(
            cfg!(target_os = "macos"),
            Modifiers::META,
            Key::Named(NamedKey::ArrowLeft),
            || {
                Some(EditingAction::MoveCursor(
                    EditingDirection::Backward,
                    EditingMotion::LineStartOrEnd,
                    update_selection,
                ))
            },
        )
        .optional_shortcut(
            cfg!(target_os = "macos"),
            Modifiers::META,
            Key::Named(NamedKey::ArrowRight),
            || {
                Some(EditingAction::MoveCursor(
                    EditingDirection::Forward,
                    EditingMotion::LineStartOrEnd,
                    update_selection,
                ))
            },
        )
        .shortcut(Modifiers::empty(), Key::Named(NamedKey::PageUp), || {
            Some(EditingAction::MoveCursor(
                EditingDirection::Backward,
                EditingMotion::Page,
                update_selection,
            ))
        })
        .shortcut(Modifiers::empty(), Key::Named(NamedKey::PageDown), || {
            Some(EditingAction::MoveCursor(
                EditingDirection::Forward,
                EditingMotion::Page,
                update_selection,
            ))
        })
        .shortcut(Modifiers::empty(), Key::Named(NamedKey::Delete), || {
            Some(EditingAction::Delete)
        })
        .shortcut(Modifiers::empty(), Key::Named(NamedKey::Backspace), || {
            Some(EditingAction::Backspace(EditingMotion::Grapheme))
        })
        .shortcut(ALT_OR_CONTROL, Key::Named(NamedKey::Backspace), || {
            Some(EditingAction::Backspace(EditingMotion::Word))
        })
        .optional_shortcut(
            cfg!(target_os = "macos"),
            Modifiers::META,
            Key::Named(NamedKey::Backspace),
            || Some(EditingAction::Backspace(EditingMotion::Line)),
        )
        .otherwise(|| match &event.key {
            Key::Character(character) if modifiers.is_empty() => {
                Some(EditingAction::InsertText(character.to_string()))
            },
            _ => None,
        })
        .flatten()
}

pub(crate) fn editing_action_from_composition_event(event: &CompositionEvent) -> EditingAction {
    match event.state {
        CompositionState::Start => EditingAction::StartComposition,
        CompositionState::Update => EditingAction::InsertCompositionText(event.data.to_string()),
        CompositionState::End => EditingAction::EndComposition(event.data.to_string()),
    }
}

/// Stillwater: Shift+arrows, Ctrl+Shift+arrows and Shift+Home/End extend a selection in the
/// page itself (not only in text fields) by moving its focus, as in Chrome and Firefox.
impl Document {
    /// Returns false when there is no selection to extend or the focus cannot move.
    pub(crate) fn extend_selection_with_keyboard(
        &self,
        cx: &mut JSContext,
        direction: EditingDirection,
        motion: EditingMotion,
    ) -> bool {
        let Some(selection) = self.GetSelection(cx) else {
            return false;
        };
        let (Some(anchor), Some(focus)) = (
            selection.composed_anchor_position(),
            selection.composed_focus_position(),
        ) else {
            return false;
        };
        let forward = direction == EditingDirection::Forward;
        let Some(focus) = self.text_position(cx.no_gc(), &focus, forward) else {
            return false;
        };
        let new_focus = match motion {
            EditingMotion::Character | EditingMotion::Grapheme => {
                self.position_after_character(cx.no_gc(), &focus, forward)
            },
            EditingMotion::Word => self.position_after_word(cx, &focus, forward),
            EditingMotion::LineStartOrEnd => self.line_edge_position(cx, &focus, forward),
            EditingMotion::Line => self.position_on_adjacent_line(cx, &focus, forward),
            EditingMotion::Page | EditingMotion::DocumentStartOrEnd => None,
        };
        let Some(new_focus) = new_focus else {
            return false;
        };
        selection.set_anchor_and_focus(cx, &anchor, &new_focus);
        true
    }

    /// The position of `boundary` as an offset in a rendered text node: the boundary itself
    /// when it is in one, otherwise the nearest rendered text in the direction of travel.
    fn text_position(
        &self,
        no_gc: &NoGC,
        boundary: &RootedSelectionBoundary,
        forward: bool,
    ) -> Option<RootedSelectionBoundary> {
        if boundary.container.is::<Text>() {
            return Some(boundary.clone());
        }
        let child = boundary.container.children().nth(boundary.offset as usize);
        if forward {
            let start = match child {
                Some(child) if is_rendered_text(&child) => {
                    return Some(RootedSelectionBoundary::new_with_utf16_offset(child, 0));
                },
                Some(child) => child,
                None => {
                    // After the container's last child: start after its last descendant.
                    let mut last = boundary.container.clone();
                    while let Some(child) = last.GetLastChild() {
                        last = child;
                    }
                    last
                },
            };
            let text = self.adjacent_rendered_text(no_gc, &start, true)?;
            Some(RootedSelectionBoundary::new_with_utf16_offset(text, 0))
        } else {
            if let Some(child) = child {
                let text = self.adjacent_rendered_text(no_gc, &child, false)?;
                let length = text.len();
                return Some(RootedSelectionBoundary::new_with_utf16_offset(text, length));
            }
            let mut last = boundary.container.clone();
            while let Some(child) = last.GetLastChild() {
                last = child;
            }
            let text = if is_rendered_text(&last) {
                last
            } else {
                self.adjacent_rendered_text(no_gc, &last, false)?
            };
            let length = text.len();
            Some(RootedSelectionBoundary::new_with_utf16_offset(text, length))
        }
    }

    /// The next (or previous) text node in tree order that is rendered and selectable.
    fn adjacent_rendered_text(
        &self,
        _no_gc: &NoGC,
        node: &Node,
        forward: bool,
    ) -> Option<DomRoot<Node>> {
        let root = self.upcast::<Node>();
        if forward {
            node.following_nodes(root, ShadowIncluding::No)
                .find(|candidate| is_rendered_text(candidate))
        } else {
            node.preceding_nodes(root)
                .find(|candidate| is_rendered_text(candidate))
        }
    }

    /// One character on from the text position `position`, crossing into the adjacent
    /// rendered text node at a node boundary. Between two blocks the boundary itself is the
    /// character (the line break), as in Chrome.
    fn position_after_character(
        &self,
        no_gc: &NoGC,
        position: &RootedSelectionBoundary,
        forward: bool,
    ) -> Option<RootedSelectionBoundary> {
        let node = &position.container;
        let data = node.downcast::<CharacterData>()?.data().to_string();
        let offset = position.offset as usize;
        if forward {
            if let Some(length) = character_length_after(&data, offset) {
                return Some(RootedSelectionBoundary::new_with_utf16_offset(
                    node.clone(),
                    (offset + length) as u32,
                ));
            }
            let next = self.adjacent_rendered_text(no_gc, node, true)?;
            let step = if rendered_text_separator_is_nothing(no_gc, node, &next) {
                character_length_after(&next.downcast::<CharacterData>()?.data(), 0).unwrap_or(0)
            } else {
                0
            };
            Some(RootedSelectionBoundary::new_with_utf16_offset(next, step as u32))
        } else {
            if let Some(length) = character_length_before(&data, offset) {
                return Some(RootedSelectionBoundary::new_with_utf16_offset(
                    node.clone(),
                    (offset - length) as u32,
                ));
            }
            let previous = self.adjacent_rendered_text(no_gc, node, false)?;
            let previous_data = previous.downcast::<CharacterData>()?.data().to_string();
            let length = utf16_length(&previous_data);
            let step = if rendered_text_separator_is_nothing(no_gc, &previous, node) {
                character_length_before(&previous_data, length).unwrap_or(0)
            } else {
                0
            };
            Some(RootedSelectionBoundary::new_with_utf16_offset(
                previous,
                (length - step) as u32,
            ))
        }
    }

    /// To the end of the next word (forward) or the start of the previous one (backward),
    /// skipping the spaces and punctuation in between.
    fn position_after_word(
        &self,
        cx: &mut JSContext,
        position: &RootedSelectionBoundary,
        forward: bool,
    ) -> Option<RootedSelectionBoundary> {
        // Skip characters that are not part of a word, crossing text nodes.
        let mut current = position.clone();
        loop {
            let data = current.container.downcast::<CharacterData>()?.data().to_string();
            let character = if forward {
                character_after(&data, current.offset as usize)
            } else {
                character_before(&data, current.offset as usize)
            };
            match character {
                Some(character) if !character.is_alphanumeric() && character != '_' => {
                    current = self.position_after_character(cx.no_gc(), &current, forward)?;
                },
                Some(_) => break,
                None => {
                    // At a node boundary: carry on in the adjacent text node.
                    let next = self.adjacent_rendered_text(cx.no_gc(), &current.container, forward)?;
                    let offset = if forward { 0 } else { next.len() };
                    current = RootedSelectionBoundary::new_with_utf16_offset(next, offset);
                },
            }
        }
        // Then to the edge of the word there.
        let data = current.container.downcast::<CharacterData>()?.data().to_string();
        let inside_word = if forward {
            current.offset
        } else {
            current.offset - character_length_before(&data, current.offset as usize)? as u32
        };
        let inside_word =
            RootedSelectionBoundary::new_with_utf16_offset(current.container.clone(), inside_word);
        let (start, end) = self.window().query_text_segment(
            cx,
            &inside_word.container,
            inside_word.utf32_offset(),
            SegmentGranularity::Word,
        )?;
        Some(if forward { end } else { start })
    }

    /// The start or end of the visual line holding the text position `position` (or of its
    /// paragraph, when the line cannot be hit-tested).
    fn line_edge_position(
        &self,
        cx: &mut JSContext,
        position: &RootedSelectionBoundary,
        forward: bool,
    ) -> Option<RootedSelectionBoundary> {
        let (paragraph_start, paragraph_end) = self.window().query_text_segment(
            cx,
            &position.container,
            position.utf32_offset(),
            SegmentGranularity::HardLineBreak,
        )?;
        let paragraph_edge = if forward {
            paragraph_end.clone()
        } else {
            paragraph_start.clone()
        };
        let Some(caret) = self.caret_rect(cx, position) else {
            return Some(paragraph_edge);
        };
        let width = self.window().InnerWidth() as f32;
        let x = if forward { width - 2.0 } else { 1.0 };
        let y = caret.origin.y.to_f32_px() + caret.size.height.to_f32_px() / 2.0;
        let Some(hit) = self.position_at_point(cx, x, y) else {
            return Some(paragraph_edge);
        };
        // Only a position on the same paragraph counts: the hit test may land on something
        // else at the edge of the page (a sidebar, a positioned element).
        let no_gc = cx.no_gc();
        if hit.cmp(no_gc, &paragraph_start) == std::cmp::Ordering::Less ||
            hit.cmp(no_gc, &paragraph_end) == std::cmp::Ordering::Greater ||
            hit.cmp(no_gc, position) == if forward {
                std::cmp::Ordering::Less
            } else {
                std::cmp::Ordering::Greater
            }
        {
            return Some(paragraph_edge);
        }
        Some(hit)
    }

    /// The position on the line above or below the text position `position`, at the same
    /// horizontal position.
    fn position_on_adjacent_line(
        &self,
        cx: &mut JSContext,
        position: &RootedSelectionBoundary,
        forward: bool,
    ) -> Option<RootedSelectionBoundary> {
        let caret = self.caret_rect(cx, position)?;
        let x = caret.origin.x.to_f32_px();
        let top = caret.origin.y.to_f32_px();
        let height = caret.size.height.to_f32_px().max(1.0);
        let y = if forward {
            top + height * 1.5
        } else {
            top - height * 0.5
        };
        let hit = self.position_at_point(cx, x, y)?;
        let ordering = hit.cmp(cx.no_gc(), position);
        let moved = if forward {
            ordering == std::cmp::Ordering::Greater
        } else {
            ordering == std::cmp::Ordering::Less
        };
        moved.then_some(hit)
    }

    /// The zero-width rectangle of the caret at the text position `position`, in viewport
    /// coordinates.
    fn caret_rect(
        &self,
        cx: &mut JSContext,
        position: &RootedSelectionBoundary,
    ) -> Option<euclid::Rect<app_units::Au, style_traits::CSSPixel>> {
        let window = self.window();
        window.layout_reflow(cx, QueryMsg::BoxAreas);
        let offset = servo_base::text::Utf32CodeUnits(position.utf32_offset().0);
        window
            .layout()
            .query_text_range_rects(position.container.to_trusted_node_address(), offset..offset)
            .into_iter()
            .next()
    }

    /// The selection position that a click at (`x`, `y`) in the viewport would give.
    fn position_at_point(
        &self,
        cx: &mut JSContext,
        x: f32,
        y: f32,
    ) -> Option<RootedSelectionBoundary> {
        let height = self.window().InnerHeight() as f32;
        if y < 0.0 || y >= height {
            return None;
        }
        self.window()
            .hit_test_from_point_in_viewport(
                cx,
                HitTestFlags::IncludeDomPosition,
                euclid::Point2D::new(x, y),
            )?
            .dom_position_for_selection
    }
}

fn is_rendered_text(node: &Node) -> bool {
    node.is::<Text>() &&
        !node.is_in_ua_widget() &&
        node.downcast::<CharacterData>()
            .and_then(|text| text.rendered_text(RangeAny::new(None, None)))
            .is_some_and(|text| !text.trim().is_empty())
}

fn rendered_text_separator_is_nothing(no_gc: &NoGC, previous: &Node, next: &Node) -> bool {
    crate::dom::selection::rendered_text_separator_is_nothing(no_gc, previous, next)
}

fn utf16_length(text: &str) -> usize {
    text.encode_utf16().count()
}

/// The UTF-16 byte-free helpers below take and return UTF-16 offsets into `text`.
fn character_after(text: &str, utf16_offset: usize) -> Option<char> {
    let mut seen = 0;
    for character in text.chars() {
        if seen == utf16_offset {
            return Some(character);
        }
        seen += character.len_utf16();
        if seen > utf16_offset {
            return None;
        }
    }
    None
}

fn character_before(text: &str, utf16_offset: usize) -> Option<char> {
    let mut seen = 0;
    let mut previous = None;
    for character in text.chars() {
        if seen == utf16_offset {
            return previous;
        }
        seen += character.len_utf16();
        previous = Some(character);
    }
    (seen == utf16_offset).then_some(previous).flatten()
}

fn character_length_after(text: &str, utf16_offset: usize) -> Option<usize> {
    character_after(text, utf16_offset).map(char::len_utf16)
}

fn character_length_before(text: &str, utf16_offset: usize) -> Option<usize> {
    character_before(text, utf16_offset).map(char::len_utf16)
}
