// Copyright 2020 The Jujutsu Authors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// https://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.
use std::collections::HashMap;
use std::io::Write as _;

use clap_complete::ArgValueCandidates;
use clap_complete::ArgValueCompleter;
use jj_lib::backend::CommitId;
use jj_lib::commit::Commit;
use jj_lib::matchers::Matcher;
use jj_lib::merge::Diff;
use jj_lib::merge::Merge;
use jj_lib::merged_tree::MergedTree;
use jj_lib::object_id::ObjectId as _;
use jj_lib::repo::Repo;
use jj_lib::rewrite::CommitRewriter;
use jj_lib::rewrite::CommitWithSelection;
use jj_lib::rewrite::EmptyBehavior;
use jj_lib::rewrite::MoveCommitsLocation;
use jj_lib::rewrite::MoveCommitsTarget;
use jj_lib::rewrite::RebaseOptions;
use jj_lib::rewrite::RebasedCommit;
use jj_lib::rewrite::RewriteRefsOptions;
use jj_lib::rewrite::move_commits;
use tracing::instrument;

use crate::cli_util::CommandHelper;
use crate::cli_util::DiffSelector;
use crate::cli_util::RevisionArg;
use crate::cli_util::WorkspaceCommandHelper;
use crate::cli_util::WorkspaceCommandTransaction;
use crate::cli_util::compute_commit_location;
use crate::cli_util::print_unmatched_explicit_paths;
use crate::command_error::CommandError;
use crate::complete;
use crate::description_util::add_trailers;
use crate::description_util::description_template;
use crate::description_util::edit_description;
use crate::description_util::join_message_paragraphs;
use crate::ui::Ui;

/// Split a revision in two
///
/// Starts a [diff editor] on the changes in the revision. Edit the right side
/// of the diff until it has the content you want in the first commit. Once you
/// close the editor, your revision will be split into two commits.
///
/// [diff editor]:
///     https://docs.jj-vcs.dev/latest/config/#editing-diffs
///
/// By default, the selected changes stay in the original commit, and the
/// remaining changes go into a new child commit:
///
/// ```text
/// L                 L'
/// |                 |
/// K (split)   =>    K" (remaining)
/// |                 |
/// J                 K' (selected)
///                   |
///                   J
/// ```
///
/// With `--parallel/-p`, the two parts become sibling commits instead of
/// parent and child:
///
/// ```text
///                   L'
/// L                / \
/// |               K'  |  (selected)
/// K (split)  =>   |   K" (remaining)
/// |                \ /
/// J                 J
/// ```
///
/// With `-o`, `-A`, or `-B`, the selected changes are extracted into a new
/// commit at the specified location, while the remaining changes stay in place:
///
/// ```text
/// M                 M'
/// |                 |
/// L                 L'
/// |                 |
/// K (split)   =>    K' (remaining, stays here)
/// |                 |
/// J                 J'
///                   |
///                   K" (selected, inserted before J with -B J)
/// ```
///
/// If the change you split had a description, you will be asked to enter a
/// change description for each commit. If the change did not have a
/// description, the second commit will not get a description, and you will be
/// asked for a description only for the first commit.
///
/// Splitting an empty commit is not supported because the same effect can be
/// achieved with `jj new`.
#[derive(clap::Args, Clone, Debug)]
#[command(verbatim_doc_comment)]
pub(crate) struct SplitArgs {
    /// Interactively choose which parts to split
    ///
    /// This is the default if no filesets are provided.
    #[arg(long, short)]
    interactive: bool,

    /// Specify diff editor to be used (implies --interactive)
    #[arg(long, value_name = "NAME")]
    #[arg(add = ArgValueCandidates::new(complete::diff_editors))]
    tool: Option<String>,

    /// The revision to split
    #[arg(long, short, default_value = "@", value_name = "REVSET")]
    #[arg(add = ArgValueCompleter::new(complete::revset_expression_mutable))]
    revision: RevisionArg,

    /// The revision(s) to rebase the selected changes onto (can be repeated to
    /// create a merge commit)
    ///
    /// Extracts the selected changes into a new commit based on the given
    /// revision(s). The remaining changes stay in the original commit's
    /// location.
    #[arg(
        long,
        visible_alias = "destination",
        short,
        visible_short_alias = 'd',
        conflicts_with = "parallel",
        value_name = "REVSETS"
    )]
    #[arg(add = ArgValueCompleter::new(complete::revset_expression_all))]
    onto: Option<Vec<RevisionArg>>,

    /// The revision(s) to insert after (can be repeated to create a merge
    /// commit)
    ///
    /// Extracts the selected changes into a new commit inserted after the
    /// given revision(s). The remaining changes stay in the original commit's
    /// location.
    #[arg(
        long,
        short = 'A',
        visible_alias = "after",
        conflicts_with_all = ["onto", "parallel"],
        value_name = "REVSETS"
    )]
    #[arg(add = ArgValueCompleter::new(complete::revset_expression_all))]
    insert_after: Option<Vec<RevisionArg>>,

    /// The revision(s) to insert before (can be repeated to create a merge
    /// commit)
    ///
    /// Extracts the selected changes into a new commit inserted before the
    /// given revision(s). The remaining changes stay in the original commit's
    /// location.
    #[arg(
        long,
        short = 'B',
        visible_alias = "before",
        conflicts_with_all = ["onto", "parallel"],
        value_name = "REVSETS"
    )]
    #[arg(add = ArgValueCompleter::new(complete::revset_expression_mutable))]
    insert_before: Option<Vec<RevisionArg>>,

    /// The change description to use for the selected changes (don't open
    /// editor)
    ///
    /// Sets the description for the revision containing the selected changes.
    /// The other revision will keep its original description, if any.
    #[arg(long = "message", short, value_name = "MESSAGE")]
    message_paragraphs: Option<Vec<String>>,

    /// Open an editor to edit the change description(s)
    ///
    /// Forces an editor to open when using `--message` to allow the message to
    /// be edited afterward.
    #[arg(long)]
    editor: bool,

    /// Split the revision into two parallel revisions instead of a parent and
    /// child
    #[arg(long, short)]
    parallel: bool,

    /// Files matching any of these filesets are put in the selected changes
    #[arg(value_name = "FILESETS", value_hint = clap::ValueHint::AnyPath)]
    #[arg(add = ArgValueCompleter::new(complete::modified_revision_files))]
    paths: Vec<String>,
}

impl SplitArgs {
    /// Resolves the raw SplitArgs into the components necessary to run the
    /// command. Returns an error if the command cannot proceed.
    async fn resolve(
        &self,
        ui: &Ui,
        workspace_command: &WorkspaceCommandHelper,
    ) -> Result<ResolvedSplitArgs, CommandError> {
        let target_commit = workspace_command
            .resolve_single_rev(ui, &self.revision)
            .await?;
        workspace_command
            .check_rewritable([target_commit.id()])
            .await?;
        let repo = workspace_command.repo();
        let fileset_expression = workspace_command.parse_file_patterns(ui, &self.paths)?;
        let matcher = fileset_expression.to_matcher();
        let diff_selector = workspace_command.diff_selector(
            ui,
            self.tool.as_deref(),
            self.interactive || self.paths.is_empty(),
        )?;
        let use_move_flags =
            self.onto.is_some() || self.insert_after.is_some() || self.insert_before.is_some();
        let (new_parent_ids, new_child_ids) = if use_move_flags {
            compute_commit_location(
                ui,
                workspace_command,
                self.onto.as_deref(),
                self.insert_after.as_deref(),
                self.insert_before.as_deref(),
                "split-out commit",
            )
            .await?
        } else {
            Default::default()
        };

        print_unmatched_explicit_paths(
            ui,
            workspace_command,
            &fileset_expression,
            [
                // We check the parent commit to account for deleted files.
                &target_commit.parent_tree(repo.as_ref()).await?,
                &target_commit.tree(),
            ],
        )?;

        Ok(ResolvedSplitArgs {
            target_commit,
            matcher,
            diff_selector,
            parallel: self.parallel,
            use_move_flags,
            new_parent_ids,
            new_child_ids,
        })
    }
}

struct ResolvedSplitArgs {
    target_commit: Commit,
    matcher: Box<dyn Matcher>,
    diff_selector: DiffSelector,
    parallel: bool,
    use_move_flags: bool,
    new_parent_ids: Vec<CommitId>,
    new_child_ids: Vec<CommitId>,
}

#[instrument(skip_all)]
pub(crate) async fn cmd_split(
    ui: &mut Ui,
    command: &CommandHelper,
    args: &SplitArgs,
) -> Result<(), CommandError> {
    let mut workspace_command = command.workspace_helper(ui)?;
    let ResolvedSplitArgs {
        target_commit,
        matcher,
        diff_selector,
        parallel,
        use_move_flags,
        new_parent_ids,
        new_child_ids,
    } = args.resolve(ui, &workspace_command).await?;
    let text_editor = workspace_command.text_editor()?;
    let mut tx = workspace_command.start_transaction();

    // Prompt the user to select the changes they want for the first commit.
    let target = select_diff(ui, &tx, &target_commit, &matcher, &diff_selector).await?;

    // Create the first commit, which includes the changes selected by the user.
    let first_commit = {
        let mut commit_builder = tx.repo_mut().rewrite_commit(&target.commit).detach();
        commit_builder.set_tree(target.selected_tree.clone());
        if use_move_flags {
            commit_builder.clear_rewrite_source();
            // Generate a new change id so that the commit being split doesn't
            // become divergent.
            commit_builder.generate_new_change_id();
        }
        let use_editor = args.message_paragraphs.is_none() || args.editor;
        let description = match &args.message_paragraphs {
            Some(paragraphs) => join_message_paragraphs(paragraphs),
            None => commit_builder.description().to_owned(),
        };
        // The first trailer would become the first line of the description.
        // Also, a commit with no description is treated in a special way in
        // jujutsu: it can be discarded as soon as it's no longer the working
        // copy. Adding a trailer to an empty description would break that
        // logic.
        let description = if !description.is_empty() || use_editor {
            commit_builder.set_description(description);
            add_trailers(ui, &tx, &commit_builder).await?
        } else {
            description
        };
        let description = if use_editor {
            commit_builder.set_description(description);
            let temp_commit = commit_builder.write_hidden().await?;
            let intro = "Enter a description for the selected changes.";
            let template = description_template(ui, &tx, intro, &temp_commit)?;
            edit_description(&text_editor, &template)?
        } else {
            description
        };
        commit_builder.set_description(description);
        commit_builder.write(tx.repo_mut()).await?
    };

    // Create the second commit, which includes everything the user didn't
    // select.
    let second_commit = {
        let target_tree = target.commit.tree();
        let new_tree = if parallel {
            // Merge the original commit tree with its parent using the tree
            // containing the user selected changes as the base for the merge.
            // This results in a tree with the changes the user didn't select.
            let selected_diff = target
                .diff_with_labels(
                    "parents of split revision",
                    "selected changes for split",
                    "split revision",
                )
                .await?;
            MergedTree::merge(Merge::from_diffs(
                (
                    target_tree,
                    format!("split revision ({})", target.commit.conflict_label()),
                ),
                [selected_diff.invert()],
            ))
            .await?
        } else {
            target_tree
        };
        let parents = if parallel {
            target.commit.parent_ids().to_vec()
        } else {
            vec![first_commit.id().clone()]
        };
        let mut commit_builder = tx.repo_mut().rewrite_commit(&target.commit).detach();
        commit_builder.set_parents(parents).set_tree(new_tree);
        let mut show_editor = args.editor;
        if !use_move_flags {
            commit_builder.clear_rewrite_source();
            // Generate a new change id so that the commit being split doesn't
            // become divergent.
            commit_builder.generate_new_change_id();
        }
        let description = if target.commit.description().is_empty() {
            // If there was no description before, don't ask for one for the
            // second commit.
            "".to_string()
        } else {
            show_editor = show_editor || args.message_paragraphs.is_none();
            // Just keep the original message unchanged
            commit_builder.description().to_owned()
        };
        let description = if show_editor {
            let new_description = add_trailers(ui, &tx, &commit_builder).await?;
            commit_builder.set_description(new_description);
            let temp_commit = commit_builder.write_hidden().await?;
            let intro = "Enter a description for the remaining changes.";
            let template = description_template(ui, &tx, intro, &temp_commit)?;
            edit_description(&text_editor, &template)?
        } else {
            description
        };
        commit_builder.set_description(description);
        commit_builder.write(tx.repo_mut()).await?
    };

    let original_description = target.commit.description();
    let first_desc = first_commit.description();
    let second_desc = second_commit.description();

    let winner = if first_desc == original_description && second_desc != original_description {
        // First commit wins
        if let Some(mut formatter) = ui.status_formatter() {
            writeln!(formatter, "Associating original change with first commit")?;
        }
        0
    } else if second_desc == original_description && first_desc != original_description {
        // Second commit wins
        if let Some(mut formatter) = ui.status_formatter() {
            writeln!(formatter, "Associating original change with second commit")?;
        }
        1
    } else if ui.can_prompt() {
        let choices = &["First commit", "Second commit", "Neither commit"];
        let choice = ui.prompt_choice(
            "Choose which commit to associate with the change",
            choices,
            Some(0), // Default to first commit to preserve old behavior
        )?;
        match choice {
            0 => 0,
            1 => 1,
            2 => 2,
            _ => unreachable!(),
        }
    } else {
        0 // Fallback to old behavior (first commit wins)
    };

    let (first_commit, second_commit) = if winner == 0 {
        (first_commit, second_commit)
    } else {
        // The first commit has the wrong ID. We need to rewrite both commits,
        // regardless of whether the second commit gets the original ID.

        // Abandon the old commits to avoid divergence (we won't need them).
        tx.repo_mut().record_abandoned_commit_with_parents(first_commit.id().clone(), first_commit.parent_ids().to_vec());
        tx.repo_mut().record_abandoned_commit_with_parents(second_commit.id().clone(), second_commit.parent_ids().to_vec());

        // first_commit gets a brand new ID in all cases.
        let mut first_builder = tx.repo_mut().rewrite_commit(&target.commit).detach();
        first_builder.set_tree(first_commit.tree().clone());
        first_builder.set_description(first_commit.description().to_owned());
        first_builder.clear_rewrite_source();
        first_builder.generate_new_change_id();
        // Overwrite first_commit
        let new_first_commit = first_builder.write(tx.repo_mut()).await?;
        
        // second_commit either keeps the original ID or gets a new one.
        // Either way, ensure we have the right parent its parent to be the new
        // first commit if not parallel!
        let parents = if parallel {
            second_commit.parent_ids().to_vec()
        } else {
            vec![new_first_commit.id().clone()]
        };

        let mut second_builder = tx.repo_mut().rewrite_commit(&target.commit).detach();
        second_builder.set_parents(parents).set_tree(second_commit.tree().clone());
        second_builder.set_description(second_commit.description().to_owned());

        if winner == 2 {
            // Note: if we skip this (i.e. winner == 1), then the change id just
            // comes from target.commit, which is what we want in that case.
            second_builder.clear_rewrite_source();
            second_builder.generate_new_change_id();
        }
        let new_second_commit = second_builder.write(tx.repo_mut()).await?;

        (new_first_commit, new_second_commit)

    let (first_commit, second_commit, num_rebased) = if use_move_flags {
        move_first_commit(
            &mut tx,
            &target,
            first_commit,
            second_commit,
            new_parent_ids,
            new_child_ids,
        )
        .await?
    } else {
        rewrite_descendants(&mut tx, &target, first_commit, second_commit, parallel, winner).await?
    };

    // Manually move bookmarks pointing to the original commit to the winner
    // commit, to ensure they follow the Change ID even if rewrite_descendants
    // moved them differently.
    if winner != 2 {
        let winner_commit = if winner == 0 { &first_commit } else { &second_commit };
        let mut bookmarks_to_move = Vec::new();
        for (bookmark, local_target) in tx.repo().view().local_bookmarks() {
            if local_target.added_ids().any(|id| id == target.commit.id()) {
                bookmarks_to_move.push(bookmark.to_owned());
            }
        }
        for bookmark in bookmarks_to_move {
            tx.repo_mut().set_local_bookmark_target(
                &bookmark,
                jj_lib::op_store::RefTarget::normal(winner_commit.id().clone()),
            );
        }
    }

    if let Some(mut formatter) = ui.status_formatter() {
        if num_rebased > 0 {
            writeln!(formatter, "Rebased {num_rebased} descendant commits")?;
        }
        write!(formatter, "Selected changes : ")?;
        tx.write_commit_summary(formatter.as_mut(), &first_commit)?;
        write!(formatter, "\nRemaining changes: ")?;
        tx.write_commit_summary(formatter.as_mut(), &second_commit)?;
        writeln!(formatter)?;
    }
    tx.finish(ui, format!("split commit {}", target.commit.id().hex()))
        .await?;
    Ok(())
}

async fn move_first_commit(
    tx: &mut WorkspaceCommandTransaction<'_>,
    target: &CommitWithSelection,
    mut first_commit: Commit,
    mut second_commit: Commit,
    new_parent_ids: Vec<CommitId>,
    new_child_ids: Vec<CommitId>,
) -> Result<(Commit, Commit, usize), CommandError> {
    let mut rewritten_commits: HashMap<CommitId, CommitId> = HashMap::new();
    rewritten_commits.insert(target.commit.id().clone(), second_commit.id().clone());
    tx.repo_mut()
        .transform_descendants(
            vec![target.commit.id().clone()],
            async |rewriter: CommitRewriter<'_>| {
                let old_commit_id = rewriter.old_commit().id().clone();
                let new_commit = rewriter.rebase().await?.write().await?;
                rewritten_commits.insert(old_commit_id, new_commit.id().clone());
                Ok(())
            },
        )
        .await?;

    let new_parent_ids: Vec<_> = new_parent_ids
        .iter()
        .map(|commit_id| rewritten_commits.get(commit_id).unwrap_or(commit_id))
        .cloned()
        .collect();
    let new_child_ids: Vec<_> = new_child_ids
        .iter()
        .map(|commit_id| rewritten_commits.get(commit_id).unwrap_or(commit_id))
        .cloned()
        .collect();
    let stats = move_commits(
        tx.repo_mut(),
        &MoveCommitsLocation {
            new_parent_ids,
            new_child_ids,
            target: MoveCommitsTarget::Commits(vec![first_commit.id().clone()]),
        },
        &RebaseOptions {
            empty: EmptyBehavior::Keep,
            rewrite_refs: RewriteRefsOptions {
                delete_abandoned_bookmarks: false,
            },
            simplify_ancestor_merge: false,
        },
    )
    .await?;

    // 1 for the transformation of the original commit to the second commit
    // that was inserted in rewritten_commits
    let mut num_new_rebased = 1;
    if let Some(RebasedCommit::Rewritten(commit)) = stats.rebased_commits.get(first_commit.id()) {
        first_commit = commit.clone();
        num_new_rebased += 1;
    }
    if let Some(RebasedCommit::Rewritten(commit)) = stats.rebased_commits.get(second_commit.id()) {
        second_commit = commit.clone();
    }

    let num_rebased = rewritten_commits.len() + stats.rebased_commits.len()
        // don't count the commit generated by the split in the rebased commits
        - num_new_rebased
        // only count once a commit that may have been rewritten twice in the process
        - rewritten_commits
            .iter()
            .filter(|(_, rewritten)| stats.rebased_commits.contains_key(rewritten))
            .count();

    Ok((first_commit, second_commit, num_rebased))
}

async fn rewrite_descendants(
    tx: &mut WorkspaceCommandTransaction<'_>,
    target: &CommitWithSelection,
    first_commit: Commit,
    second_commit: Commit,
    parallel: bool,
    winner: usize,
) -> Result<(Commit, Commit, usize), CommandError> {
    let legacy_bookmark_behavior = tx.settings().get_bool("split.legacy-bookmark-behavior")?;
    let winner_commit = if winner == 0 { &first_commit } else { &second_commit };
    // Mark the commit being split as rewritten to the winner commit. This
    // moves any bookmarks pointing to the target commit to the winner
    // commit, following the Change ID.
    tx.repo_mut()
        .set_rewritten_commit(target.commit.id().clone(), winner_commit.id().clone());
    let mut num_rebased = 0;
    tx.repo_mut()
        .transform_descendants(
            vec![target.commit.id().clone()],
            async |mut rewriter: CommitRewriter<'_>| {
                num_rebased += 1;
                if parallel && legacy_bookmark_behavior {
                    // The old_parent is the winner commit due to the rewrite above.
                    rewriter.replace_parent(
                        winner_commit.id(),
                        [first_commit.id(), second_commit.id()],
                    );
                } else if parallel {
                    rewriter
                        .replace_parent(first_commit.id(), [first_commit.id(), second_commit.id()]);
                } else {
                    rewriter.replace_parent(first_commit.id(), [second_commit.id()]);
                }
                rewriter.rebase().await?.write().await?;
                Ok(())
            },
        )
        .await?;
    // Move the working copy commit (@) to the second commit for any workspaces
    // where the target commit is the working copy commit.
    for (name, working_copy_commit) in tx.base_repo().clone().view().wc_commit_ids() {
        if working_copy_commit == target.commit.id() {
            tx.repo_mut().edit(name.clone(), &second_commit).await?;
        }
    }

    Ok((first_commit, second_commit, num_rebased))
}

/// Prompts the user to select the content they want in the first commit and
/// returns the target commit and the tree corresponding to the selection.
async fn select_diff(
    ui: &Ui,
    tx: &WorkspaceCommandTransaction<'_>,
    target_commit: &Commit,
    matcher: &dyn Matcher,
    diff_selector: &DiffSelector,
) -> Result<CommitWithSelection, CommandError> {
    let format_instructions = || {
        format!(
            "\
You are splitting a commit into two: {}

The diff initially shows the changes in the commit you're splitting.

Adjust the right side until it shows the contents you want to split into the
new commit.
The changes that are not selected will replace the original commit.
",
            tx.format_commit_summary(target_commit)
        )
    };
    let parent_tree = target_commit.parent_tree(tx.repo()).await?;
    let selected_tree = diff_selector
        .select(
            ui,
            Diff::new(&parent_tree, &target_commit.tree()),
            Diff::new(
                target_commit.parents_conflict_label().await?,
                target_commit.conflict_label(),
            ),
            matcher,
            format_instructions,
        )
        .await?;
    let selection = CommitWithSelection {
        commit: target_commit.clone(),
        selected_tree,
        parent_tree,
    };
    if selection.is_full_selection() {
        writeln!(
            ui.warning_default(),
            "All changes have been selected, so the original revision will become empty"
        )?;
    } else if selection.is_empty_selection() {
        writeln!(
            ui.warning_default(),
            "No changes have been selected, so the new revision will be empty"
        )?;
    }

    Ok(selection)
}
