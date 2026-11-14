use crate::{
    dynamic::Factory,
    storage::attribution::Component,
    trace::{Cursor, cursor::Position, with_read_owner},
};

/// A cursor that runs each call of the cursor it wraps under a fixed read
/// owner ([`with_read_owner`]), so that the run reads it causes are counted for
/// that owner, whatever owner is current where the caller is.
///
/// A scope around a whole operation would not do for a cursor that interleaves
/// with another one, such as the partner cursor of a join, which steps in
/// between the delta cursor's own reads. This adapter charges each call alone.
pub struct ReadOwnerCursor<C> {
    owner: Component,
    cursor: C,
}

impl<C> ReadOwnerCursor<C> {
    /// Wraps `cursor`; its calls are counted for `owner`.
    pub fn new(owner: Component, cursor: C) -> Self {
        Self { owner, cursor }
    }
}

impl<K, V, T, R, C> Cursor<K, V, T, R> for ReadOwnerCursor<C>
where
    K: ?Sized,
    V: ?Sized,
    R: ?Sized,
    C: Cursor<K, V, T, R>,
{
    fn weight_factory(&self) -> &'static dyn Factory<R> {
        with_read_owner(self.owner, || self.cursor.weight_factory())
    }

    fn key_valid(&self) -> bool {
        with_read_owner(self.owner, || self.cursor.key_valid())
    }

    fn val_valid(&self) -> bool {
        with_read_owner(self.owner, || self.cursor.val_valid())
    }

    fn key(&self) -> &K {
        with_read_owner(self.owner, || self.cursor.key())
    }

    fn val(&self) -> &V {
        with_read_owner(self.owner, || self.cursor.val())
    }

    fn map_times(&mut self, logic: &mut dyn FnMut(&T, &R)) {
        with_read_owner(self.owner, || self.cursor.map_times(logic))
    }

    fn map_times_through(&mut self, upper: &T, logic: &mut dyn FnMut(&T, &R)) {
        with_read_owner(self.owner, || self.cursor.map_times_through(upper, logic))
    }

    fn weight(&mut self) -> &R
    where
        T: PartialEq<()>,
    {
        with_read_owner(self.owner, || self.cursor.weight())
    }

    fn weight_checked(&mut self) -> &R {
        with_read_owner(self.owner, || self.cursor.weight_checked())
    }

    fn map_values(&mut self, logic: &mut dyn FnMut(&V, &R))
    where
        T: PartialEq<()>,
    {
        with_read_owner(self.owner, || self.cursor.map_values(logic))
    }

    fn step_key(&mut self) {
        with_read_owner(self.owner, || self.cursor.step_key())
    }

    fn step_key_reverse(&mut self) {
        with_read_owner(self.owner, || self.cursor.step_key_reverse())
    }

    fn seek_key(&mut self, key: &K) {
        with_read_owner(self.owner, || self.cursor.seek_key(key))
    }

    fn seek_key_exact(&mut self, key: &K, hash: Option<u64>) -> bool {
        with_read_owner(self.owner, || self.cursor.seek_key_exact(key, hash))
    }

    fn seek_key_with(&mut self, predicate: &dyn Fn(&K) -> bool) {
        with_read_owner(self.owner, || self.cursor.seek_key_with(predicate))
    }

    fn seek_key_with_reverse(&mut self, predicate: &dyn Fn(&K) -> bool) {
        with_read_owner(self.owner, || self.cursor.seek_key_with_reverse(predicate))
    }

    fn seek_key_reverse(&mut self, key: &K) {
        with_read_owner(self.owner, || self.cursor.seek_key_reverse(key))
    }

    fn step_val(&mut self) {
        with_read_owner(self.owner, || self.cursor.step_val())
    }

    fn step_val_reverse(&mut self) {
        with_read_owner(self.owner, || self.cursor.step_val_reverse())
    }

    fn seek_val(&mut self, val: &V) {
        with_read_owner(self.owner, || self.cursor.seek_val(val))
    }

    fn seek_val_reverse(&mut self, val: &V) {
        with_read_owner(self.owner, || self.cursor.seek_val_reverse(val))
    }

    fn seek_val_with(&mut self, predicate: &dyn Fn(&V) -> bool) {
        with_read_owner(self.owner, || self.cursor.seek_val_with(predicate))
    }

    fn seek_val_with_reverse(&mut self, predicate: &dyn Fn(&V) -> bool) {
        with_read_owner(self.owner, || self.cursor.seek_val_with_reverse(predicate))
    }

    fn rewind_keys(&mut self) {
        with_read_owner(self.owner, || self.cursor.rewind_keys())
    }

    fn fast_forward_keys(&mut self) {
        with_read_owner(self.owner, || self.cursor.fast_forward_keys())
    }

    fn rewind_vals(&mut self) {
        with_read_owner(self.owner, || self.cursor.rewind_vals())
    }

    fn fast_forward_vals(&mut self) {
        with_read_owner(self.owner, || self.cursor.fast_forward_vals())
    }

    fn position(&self) -> Option<Position> {
        with_read_owner(self.owner, || self.cursor.position())
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        ZWeight,
        dynamic::{DowncastTrait, Erase},
        storage::attribution::Component,
        trace::{
            BatchReader as DynamicBatchReader, Cursor, RunReads, current_read_owner,
            with_read_owner,
        },
        typed_batch::{BatchReader, OrdIndexedZSet},
        utils::Tup2,
    };

    use super::ReadOwnerCursor;

    fn batch(rows: &[(i64, i64)]) -> OrdIndexedZSet<i64, i64> {
        OrdIndexedZSet::from_tuples((), rows.iter().map(|&(k, v)| Tup2(Tup2(k, v), 1)).collect())
    }

    /// The reads of `f` for `owner` and for the thread's other owners together.
    fn reads_of(owner: Component, f: impl FnOnce()) -> (RunReads, RunReads) {
        let (before, before_owner) = (RunReads::current(), RunReads::current_for(owner));
        f();
        let for_owner = RunReads::current_for(owner).since(before_owner);
        let all = RunReads::current().since(before);
        (for_owner, all)
    }

    #[test]
    fn a_wrapped_cursor_charges_its_own_reads_to_the_owner_and_nothing_else() {
        let wrapped = batch(&[(1, 10), (2, 20), (3, 30)]);
        let other = batch(&[(1, 10), (2, 20), (3, 30)]);
        let mut other_cursor = other.inner().cursor();
        let mut cursor = ReadOwnerCursor::new(Component::RequestedSlice, wrapped.inner().cursor());

        let (slice, all) = reads_of(Component::RequestedSlice, || {
            // The two cursors interleave, as a join's delta and partner cursors
            // do; only the wrapped cursor's calls are charged to the owner.
            assert!(cursor.seek_key_exact(2i64.erase(), None));
            other_cursor.step_key();
            assert_eq!(*cursor.val().downcast_checked::<i64>(), 20);
            assert_eq!(*cursor.weight().downcast_checked::<ZWeight>(), 1);
            other_cursor.step_key();
            cursor.step_key();
            assert_eq!(*other_cursor.key().downcast_checked::<i64>(), 3);
        });
        assert!(slice.memory_key_reads > 0);
        assert!(slice.memory_weight_reads > 0);
        // The other cursor's reads were made under the default owner.
        assert!(all.memory_key_reads > slice.memory_key_reads);

        // Without the adapter the same reads count for the current owner.
        let (slice_plain, _) = reads_of(Component::RequestedSlice, || {
            let mut plain = wrapped.inner().cursor();
            assert!(plain.seek_key_exact(2i64.erase(), None));
        });
        assert_eq!(slice_plain, RunReads::default());
    }

    #[test]
    fn the_adapter_overrides_the_current_owner_and_restores_it() {
        let wrapped = batch(&[(1, 10)]);
        let mut cursor = ReadOwnerCursor::new(Component::RequestedSlice, wrapped.inner().cursor());
        let (slice, index) = {
            let before = (
                RunReads::current_for(Component::RequestedSlice),
                RunReads::current_for(Component::MergedIndex),
            );
            with_read_owner(Component::MergedIndex, || {
                cursor.step_key();
                cursor.rewind_keys();
                assert_eq!(current_read_owner(), Component::MergedIndex);
            });
            (
                RunReads::current_for(Component::RequestedSlice).since(before.0),
                RunReads::current_for(Component::MergedIndex).since(before.1),
            )
        };
        assert!(slice.memory_key_reads > 0);
        assert_eq!(index, RunReads::default());
    }
}
