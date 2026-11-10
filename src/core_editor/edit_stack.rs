#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditStack<T> {
    /// States before the current one, oldest first.
    undo: Vec<T>,
    /// The state currently pointed at. Always exists, so no index bounds to check.
    current: T,
    /// States after the current one, nearest first (a stack).
    redo: Vec<T>,
}

impl<T> EditStack<T> {
    pub fn new() -> Self
    where
        T: Default,
    {
        EditStack {
            undo: Vec::new(),
            current: T::default(),
            redo: Vec::new(),
        }
    }
}

impl<T> EditStack<T>
where
    T: Default,
{
    /// Go back one point in the undo stack. If present on first edit do nothing
    pub(super) fn undo(&mut self) -> &T {
        if let Some(prev) = self.undo.pop() {
            let cur = std::mem::replace(&mut self.current, prev);
            self.redo.push(cur);
        }
        &self.current
    }

    /// Undo every recorded change, landing on the oldest state — GNU
    /// readline `rl_revert_line` (undo.c:331), which loops `rl_do_undo`
    /// until the undo list is empty.
    ///
    /// Returns `false` when there was nothing to undo, matching GNU's
    /// `rl_ding` on an empty `rl_undo_list`. The walked states stay on
    /// the redo stack (our undo keeps a redo side GNU lacks).
    ///
    /// The bottom of this stack is the current line's undo floor: the
    /// state the stack last restarted at. History navigation restarts
    /// the stack at the landed entry (see
    /// [`EditStack::restart_at`]), so a recall followed by edits
    /// reverts to the recalled text, never to the line that was live
    /// before traversal began.
    pub(super) fn undo_all(&mut self) -> bool {
        if self.undo.is_empty() {
            return false;
        }
        while let Some(prev) = self.undo.pop() {
            let cur = std::mem::replace(&mut self.current, prev);
            self.redo.push(cur);
        }
        true
    }

    /// Go forward one point in the undo stack. If present on the last edit do nothing
    pub(super) fn redo(&mut self) -> &T {
        if let Some(next) = self.redo.pop() {
            let cur = std::mem::replace(&mut self.current, next);
            self.undo.push(cur);
        }
        &self.current
    }

    /// Insert a new entry to the undo stack.
    /// NOTE: (IMP): If we have hit undo a few times then discard all the other values that come
    /// after the current point
    pub(super) fn insert(&mut self, value: T) {
        self.redo.clear();
        let prev = std::mem::replace(&mut self.current, value);
        self.undo.push(prev);
    }

    /// Reset the stack to the initial state
    pub(super) fn reset(&mut self) {
        self.undo.clear();
        self.redo.clear();
        self.current = T::default();
    }

    /// Restart the stack at `state`: it becomes the current state with
    /// empty undo and redo sides, i.e. the undo floor for everything
    /// recorded from here on.
    ///
    /// This is the history-recall boundary (issue #53): GNU installs
    /// the fetched entry's own undo list on recall (misc.c
    /// `rl_replace_from_history` — `rl_undo_list = entry->data`, empty
    /// for an entry not edited this session), so the recalled text
    /// itself is where undo and `rl_revert_line` stop. Restarting at
    /// the landed text models exactly that: nothing recorded before
    /// the landing can be reached again.
    pub(super) fn restart_at(&mut self, state: T) {
        self.undo.clear();
        self.redo.clear();
        self.current = state;
    }

    /// Return the entry currently being pointed to
    pub(super) fn current(&mut self) -> &T {
        &self.current
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use pretty_assertions::assert_eq;
    use rstest::rstest;

    fn edit_stack<T>(values: &[T], index: usize) -> EditStack<T>
    where
        T: Clone,
    {
        let (before, rest) = values.split_at(index);
        let (current, after) = rest.split_first().expect("index within values");
        EditStack {
            undo: before.to_vec(),
            current: current.clone(),
            redo: after.iter().rev().cloned().collect(),
        }
    }

    #[rstest]
    #[case(edit_stack(&[1, 2, 3][..], 2), 2)]
    #[case(edit_stack(&[1][..], 0), 1)]
    fn undo_works(#[case] stack: EditStack<isize>, #[case] value_after_undo: isize) {
        let mut stack = stack;

        let value = stack.undo();
        assert_eq!(*value, value_after_undo);
    }

    #[rstest]
    #[case(edit_stack(&[1, 2, 3][..], 1), 3)]
    #[case(edit_stack(&[1][..], 0), 1)]
    fn redo_works(#[case] stack: EditStack<isize>, #[case] value_after_undo: isize) {
        let mut stack = stack;

        let value = stack.redo();
        assert_eq!(*value, value_after_undo);
    }

    #[test]
    fn undo_all_walks_to_the_bottom() {
        let mut stack = edit_stack(&[1, 2, 3][..], 2);

        assert!(stack.undo_all());
        assert_eq!(*stack.current(), 1);
        // Every undone state stays redoable, oldest change first.
        assert_eq!(*stack.redo(), 2);
        assert_eq!(*stack.redo(), 3);
    }

    #[test]
    fn undo_all_on_empty_stack_is_a_noop() {
        let mut stack = EditStack::<isize>::new();
        assert!(!stack.undo_all());
    }

    #[test]
    fn restart_at_makes_the_state_the_undo_floor() {
        let mut stack = edit_stack(&[1, 2, 3][..], 1);

        stack.restart_at(9);
        // Nothing recorded before the restart is reachable any more:
        // undo has nothing to pop and redo nothing to replay.
        assert_eq!(*stack.current(), 9);
        assert_eq!(*stack.undo(), 9);
        assert_eq!(*stack.redo(), 9);
        assert!(!stack.undo_all());
        // Edits after the restart stack on top of the floor as usual.
        stack.insert(10);
        assert!(stack.undo_all());
        assert_eq!(*stack.current(), 9);
    }

    #[rstest]
    #[case(edit_stack(&[1, 2, 3][..], 1), 4, edit_stack(&[1, 2, 4], 2))]
    #[case(edit_stack(&[1, 2, 3][..], 2), 3, edit_stack(&[1, 2, 3, 3], 3))]
    fn insert_works(
        #[case] old_stack: EditStack<isize>,
        #[case] value_to_insert: isize,
        #[case] expected_stack: EditStack<isize>,
    ) {
        let mut stack = old_stack;

        stack.insert(value_to_insert);
        assert_eq!(stack, expected_stack);
    }
}
