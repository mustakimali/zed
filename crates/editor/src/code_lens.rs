use collections::HashMap;
use gpui::{SharedString, Task, WeakEntity};
use language::BufferId;
use multi_buffer::{Anchor, MultiBufferRow, MultiBufferSnapshot, ToPoint as _};
use project::CodeAction;
use settings::Settings as _;
use ui::{Context, Window, div, prelude::*};
use workspace::{Toast, notifications::NotificationId};

use crate::{
    Editor, FindAllReferences, GoToImplementation, SelectionEffects,
    display_map::{BlockPlacement, BlockProperties, BlockStyle, CustomBlockId},
};

struct CodeLensToast;

#[derive(Clone, Debug)]
pub struct CodeLensItem {
    pub text: SharedString,
    pub action: Option<CodeAction>,
}

#[derive(Clone, Debug)]
pub struct CodeLensData {
    pub position: Anchor,
    pub symbol_position: Anchor,
    pub items: Vec<CodeLensItem>,
}

#[derive(Default)]
pub struct CodeLensCache {
    enabled: bool,
    lenses: HashMap<BufferId, Vec<CodeLensData>>,
    pending_refresh: HashMap<BufferId, Task<()>>,
    block_ids: HashMap<BufferId, Vec<CustomBlockId>>,
}

impl CodeLensCache {
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled,
            lenses: HashMap::default(),
            pending_refresh: HashMap::default(),
            block_ids: HashMap::default(),
        }
    }

    pub fn toggle(&mut self, enabled: bool) -> bool {
        if self.enabled == enabled {
            return false;
        }
        self.enabled = enabled;
        if !enabled {
            self.clear();
        }
        true
    }

    pub fn clear(&mut self) {
        self.lenses.clear();
        self.pending_refresh.clear();
        self.block_ids.clear();
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    pub fn set_lenses_for_buffer(&mut self, buffer_id: BufferId, lenses: Vec<CodeLensData>) {
        self.lenses.insert(buffer_id, lenses);
    }

    pub fn set_block_ids(&mut self, buffer_id: BufferId, block_ids: Vec<CustomBlockId>) {
        self.block_ids.insert(buffer_id, block_ids);
    }

    pub fn get_block_ids(&self, buffer_id: &BufferId) -> Option<&Vec<CustomBlockId>> {
        self.block_ids.get(buffer_id)
    }

    pub fn all_block_ids(&self) -> Vec<CustomBlockId> {
        self.block_ids
            .values()
            .flat_map(|ids| ids.iter().copied())
            .collect()
    }

    pub fn set_refresh_task(&mut self, buffer_id: BufferId, task: Task<()>) {
        self.pending_refresh.insert(buffer_id, task);
    }

    pub fn remove_refresh_task(&mut self, buffer_id: &BufferId) {
        self.pending_refresh.remove(buffer_id);
    }
}

fn group_lenses_by_row(
    lenses: Vec<(Anchor, CodeLensItem)>,
    snapshot: &MultiBufferSnapshot,
) -> Vec<CodeLensData> {
    let mut grouped: HashMap<u32, (Anchor, Vec<CodeLensItem>)> = HashMap::default();

    for (position, item) in lenses {
        let row = position.to_point(snapshot).row;
        grouped
            .entry(row)
            .or_insert_with(|| (position, Vec::new()))
            .1
            .push(item);
    }

    let mut result: Vec<CodeLensData> = grouped
        .into_iter()
        .map(|(row, (symbol_position, items))| {
            let indent = snapshot.indent_size_for_line(MultiBufferRow(row));
            let position = snapshot.anchor_at(text::Point::new(row, indent.len), text::Bias::Left);
            CodeLensData {
                position,
                symbol_position,
                items,
            }
        })
        .collect();

    result.sort_by_key(|lens| lens.position.to_point(snapshot).row);
    result
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CodeLensKind {
    References,
    Implementations,
    Other,
}

fn detect_lens_kind(title: &str) -> CodeLensKind {
    let title_lower = title.to_lowercase();
    if title_lower.contains("reference") {
        CodeLensKind::References
    } else if title_lower.contains("implementation") {
        CodeLensKind::Implementations
    } else {
        CodeLensKind::Other
    }
}

fn should_hide_lens(title: &str) -> bool {
    title.starts_with("0 ") // 0 reference or 0 implementation
}

fn render_code_lens_line(
    lens: CodeLensData,
    editor: WeakEntity<Editor>,
) -> impl Fn(&mut crate::display_map::BlockContext) -> gpui::AnyElement {
    move |cx| {
        let mut children: Vec<gpui::AnyElement> = Vec::new();

        for (i, item) in lens.items.iter().enumerate() {
            if i > 0 {
                children.push(
                    div()
                        .text_ui_xs(cx.app)
                        .text_color(cx.app.theme().colors().text_muted)
                        .child(" | ")
                        .into_any_element(),
                );
            }

            let text = item.text.clone();
            let action = item.action.clone();
            let editor_clone = editor.clone();
            let position = lens.symbol_position;

            children.push(
                div()
                    .id(SharedString::from(format!("code-lens-{}-{}", i, text)))
                    .text_ui_xs(cx.app)
                    .text_color(cx.app.theme().colors().text_muted)
                    .cursor_pointer()
                    .hover(|style| style.text_color(cx.app.theme().colors().text))
                    .child(text.clone())
                    .on_click({
                        let text = text.clone();
                        move |_event, window, cx| {
                            let kind = detect_lens_kind(&text);
                            if let Some(editor) = editor_clone.upgrade() {
                                editor.update(cx, |editor, cx| {
                                    editor.change_selections(
                                        SelectionEffects::default(),
                                        window,
                                        cx,
                                        |s| {
                                            s.select_anchor_ranges([position..position]);
                                        },
                                    );

                                    match kind {
                                        CodeLensKind::References => {
                                            if let Some(task) = editor.find_all_references(
                                                &FindAllReferences::default(),
                                                window,
                                                cx,
                                            ) {
                                                task.detach_and_log_err(cx);
                                            }
                                        }
                                        CodeLensKind::Implementations => {
                                            editor
                                                .go_to_implementation(
                                                    &GoToImplementation,
                                                    window,
                                                    cx,
                                                )
                                                .detach_and_log_err(cx);
                                        }
                                        CodeLensKind::Other => {
                                            if let Some(action) = &action {
                                                if let Some(workspace) = editor.workspace() {
                                                    let project =
                                                        workspace.read(cx).project().clone();
                                                    let action = action.clone();
                                                    let buffer = editor.buffer().clone();
                                                    if let Some(excerpt_buffer) =
                                                        buffer.read(cx).as_singleton()
                                                    {
                                                        project
                                                            .update(cx, |project, cx| {
                                                                project.apply_code_action(
                                                                    excerpt_buffer.clone(),
                                                                    action,
                                                                    true,
                                                                    cx,
                                                                )
                                                            })
                                                            .detach_and_log_err(cx);
                                                    }
                                                }
                                            }
                                        }
                                    }
                                });
                            }
                        }
                    })
                    .into_any_element(),
            );
        }

        div()
            .size_full()
            .pl(cx.anchor_x)
            .flex()
            .flex_row()
            .items_end()
            .children(children)
            .into_any_element()
    }
}

impl Editor {
    pub fn code_lens_enabled(&self) -> bool {
        self.code_lens_cache.enabled()
    }

    pub fn refresh_code_lenses(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.code_lens_enabled() {
            return;
        }

        let buffer = self.buffer().read(cx);
        let excerpt_buffer = match buffer.as_singleton() {
            Some(b) => b,
            None => return,
        };
        let buffer_id = excerpt_buffer.read(cx).remote_id();
        let excerpt_buffer = excerpt_buffer.clone();

        let Some(project) = self.project.clone() else {
            return;
        };

        let debounce = crate::EditorSettings::get_global(cx).code_lens.debounce.0;
        let text_range = text::Anchor::min_max_range_for_buffer(buffer_id);
        let multibuffer = self.buffer().clone();

        let task = cx.spawn_in(window, async move |editor, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(debounce))
                .await;

            let actions_task = project.update(cx, |project, cx| {
                project.code_lens_actions::<text::Anchor>(&excerpt_buffer, text_range.clone(), cx)
            });

            let actions: anyhow::Result<Option<Vec<CodeAction>>> = actions_task.await;

            if let Err(e) = &actions {
                log::error!("Failed to fetch code lens actions: {e:#}");
            }

            if let Ok(Some(actions)) = actions {
                let lenses = multibuffer.update(cx, |multibuffer, cx| {
                    let snapshot = multibuffer.snapshot(cx);

                    let individual_lenses: Vec<(Anchor, CodeLensItem)> = actions
                        .into_iter()
                        .filter_map(|action| {
                            let position = snapshot.anchor_in_excerpt(action.range.start)?;

                            let text = match &action.lsp_action {
                                project::LspAction::CodeLens(lens) => {
                                    lens.command.as_ref().map(|cmd| cmd.title.clone())
                                }
                                _ => None,
                            };

                            text.and_then(|text| {
                                if should_hide_lens(&text) {
                                    None
                                } else {
                                    Some((
                                        position,
                                        CodeLensItem {
                                            text: text.into(),
                                            action: Some(action),
                                        },
                                    ))
                                }
                            })
                        })
                        .collect();

                    group_lenses_by_row(individual_lenses, &snapshot)
                });

                if let Err(e) = editor.update(cx, |editor, cx| {
                    if !editor.code_lens_cache.enabled() {
                        return;
                    }

                    if let Some(old_block_ids) = editor.code_lens_cache.get_block_ids(&buffer_id) {
                        editor.remove_blocks(old_block_ids.iter().copied().collect(), None, cx);
                    }

                    editor
                        .code_lens_cache
                        .set_lenses_for_buffer(buffer_id, lenses.clone());

                    let editor_handle = cx.entity().downgrade();

                    let blocks = lenses
                        .into_iter()
                        .map(|lens| {
                            let position = lens.position;
                            let render_fn = render_code_lens_line(lens, editor_handle.clone());
                            BlockProperties {
                                placement: BlockPlacement::Above(position),
                                height: Some(1),
                                style: BlockStyle::Sticky,
                                render: std::sync::Arc::new(render_fn),
                                priority: 0,
                            }
                        })
                        .collect::<Vec<_>>();

                    let block_ids = editor.insert_blocks(blocks, None, cx);
                    editor.code_lens_cache.set_block_ids(buffer_id, block_ids);
                    if let Some(workspace) = editor.workspace() {
                        workspace.update(cx, |workspace, cx| {
                            workspace.dismiss_notification(
                                &NotificationId::unique::<CodeLensToast>(),
                                cx,
                            );
                        });
                    }
                    cx.notify();
                }) {
                    editor
                        .update(cx, |editor, _cx| {
                            editor.code_lens_cache.remove_refresh_task(&buffer_id);
                        })
                        .ok();
                    log::error!("Failed to update code lens blocks: {e:#}");
                    return;
                }
            }

            editor
                .update(cx, |editor, cx| {
                    editor.code_lens_cache.remove_refresh_task(&buffer_id);
                    if let Some(workspace) = editor.workspace() {
                        workspace.update(cx, |workspace, cx| {
                            workspace.dismiss_notification(
                                &NotificationId::unique::<CodeLensToast>(),
                                cx,
                            );
                        });
                    }
                })
                .ok();
        });

        self.code_lens_cache.set_refresh_task(buffer_id, task);
    }

    pub fn toggle_code_lenses(
        &mut self,
        _: &crate::actions::ToggleCodeLens,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let enabled = !self.code_lens_cache.enabled();
        if enabled {
            self.code_lens_cache.toggle(enabled);
            self.refresh_code_lenses(window, cx);
            if let Some(workspace) = self.workspace() {
                workspace.update(cx, |workspace, cx| {
                    workspace.show_toast(
                        Toast::new(
                            NotificationId::unique::<CodeLensToast>(),
                            "Code lens enabled, loading...",
                        ),
                        cx,
                    );
                });
            }
        } else {
            let all_block_ids = self.code_lens_cache.all_block_ids();
            self.code_lens_cache.toggle(enabled);
            if !all_block_ids.is_empty() {
                self.remove_blocks(all_block_ids.into_iter().collect(), None, cx);
            }
            if let Some(workspace) = self.workspace() {
                workspace.update(cx, |workspace, cx| {
                    workspace.dismiss_notification(&NotificationId::unique::<CodeLensToast>(), cx);
                });
            }
        }
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor_tests::init_test;
    use crate::test::editor_lsp_test_context::EditorLspTestContext;
    use gpui::TestAppContext;
    use indoc::indoc;

    #[test]
    fn test_detect_lens_kind() {
        assert_eq!(detect_lens_kind("3 references"), CodeLensKind::References);
        assert_eq!(detect_lens_kind("1 reference"), CodeLensKind::References);
        assert_eq!(
            detect_lens_kind("2 implementations"),
            CodeLensKind::Implementations
        );
        assert_eq!(
            detect_lens_kind("1 implementation"),
            CodeLensKind::Implementations
        );
        assert_eq!(detect_lens_kind("Run test"), CodeLensKind::Other);
        assert_eq!(detect_lens_kind("Debug"), CodeLensKind::Other);
    }

    #[test]
    fn test_should_hide_lens() {
        assert!(should_hide_lens("0 references"));
        assert!(should_hide_lens("0 implementations"));
        assert!(!should_hide_lens("1 reference"));
        assert!(!should_hide_lens("3 references"));
        assert!(!should_hide_lens("Run test"));
    }

    #[test]
    fn test_cache_toggle() {
        let mut cache = CodeLensCache::new(false);
        assert!(!cache.enabled());

        cache.toggle(true);
        assert!(cache.enabled());

        cache.toggle(false);
        assert!(!cache.enabled());
        assert!(cache.all_block_ids().is_empty());
    }

    #[test]
    fn test_cache_toggle_clears_state() {
        let mut cache = CodeLensCache::new(true);
        cache.set_lenses_for_buffer(BufferId::new(1).unwrap(), vec![]);
        cache.set_block_ids(
            BufferId::new(1).unwrap(),
            vec![CustomBlockId(1)],
        );

        assert_eq!(cache.all_block_ids(), vec![CustomBlockId(1)]);

        cache.toggle(false);
        assert!(cache.all_block_ids().is_empty());
        assert!(cache.get_block_ids(&BufferId::new(1).unwrap()).is_none());
    }

    #[test]
    fn test_cache_all_block_ids_across_buffers() {
        let mut cache = CodeLensCache::new(true);
        cache.set_block_ids(
            BufferId::new(1).unwrap(),
            vec![CustomBlockId(1), CustomBlockId(2)],
        );
        cache.set_block_ids(
            BufferId::new(2).unwrap(),
            vec![CustomBlockId(3)],
        );

        let mut all = cache.all_block_ids();
        all.sort_by_key(|id| id.0);
        assert_eq!(
            all,
            vec![CustomBlockId(1), CustomBlockId(2), CustomBlockId(3)]
        );
    }

    #[gpui::test]
    async fn test_code_lens_toggle(cx: &mut TestAppContext) {
        init_test(cx, |_| {});

        let mut cx = EditorLspTestContext::new_rust(
            lsp::ServerCapabilities {
                code_lens_provider: Some(lsp::CodeLensOptions {
                    resolve_provider: Some(false),
                }),
                ..Default::default()
            },
            cx,
        )
        .await;

        cx.set_state(indoc! {"
            struct Fooˇ;
        "});

        // Initially disabled
        cx.editor(|editor, _, _cx| {
            assert!(!editor.code_lens_enabled());
        });

        // Toggle on
        cx.update_editor(|editor, window, cx| {
            editor.toggle_code_lenses(&crate::actions::ToggleCodeLens, window, cx);
            assert!(editor.code_lens_enabled());
        });

        // Toggle off
        cx.update_editor(|editor, window, cx| {
            editor.toggle_code_lenses(&crate::actions::ToggleCodeLens, window, cx);
            assert!(!editor.code_lens_enabled());
        });
    }

    #[gpui::test]
    async fn test_code_lens_toggle_on_off_clears_blocks(cx: &mut TestAppContext) {
        init_test(cx, |_| {});

        let mut cx = EditorLspTestContext::new_rust(
            lsp::ServerCapabilities {
                code_lens_provider: Some(lsp::CodeLensOptions {
                    resolve_provider: Some(false),
                }),
                ..Default::default()
            },
            cx,
        )
        .await;

        cx.set_state(indoc! {"
            fn helloˇ() {}
        "});

        // Toggle on then off - should not leave stale blocks
        cx.update_editor(|editor, window, cx| {
            editor.toggle_code_lenses(&crate::actions::ToggleCodeLens, window, cx);
        });
        cx.update_editor(|editor, window, cx| {
            editor.toggle_code_lenses(&crate::actions::ToggleCodeLens, window, cx);
            assert!(!editor.code_lens_enabled());
            assert!(editor.code_lens_cache.all_block_ids().is_empty());
        });
    }
}
