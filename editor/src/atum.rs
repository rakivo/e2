use std::hash::Hash;
use std::sync::atomic::{AtomicU32, Ordering};

use dashmap::DashMap;
use ecow::EcoString;
use papaya::LocalGuard;
use rustc_hash::FxBuildHasher;
use cranelift_entity::packed_option::ReservedValue;

impl nohash_hasher::IsEnabled for Atom {}

type NoHashDashMap<K, V> = DashMap<K, V, nohash_hasher::BuildNoHashHasher<K>>;
type FxHashPapayaHashMap<K, V> = papaya::HashMap<K, V, rustc_hash::FxBuildHasher>;
type StrToIdMapRef<'map, 'g> =
    papaya::HashMapRef<'map, EcoString, Atom, FxBuildHasher, LocalGuard<'g>>;

/// Strongly-typed atom id.
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)]
pub struct Atom(pub u32);
cranelift_entity::entity_impl!(Atom);

impl Default for Atom {
    fn default() -> Self {
        Self::reserved_value()
    }
}

/// Lock-free, bidirectional string atom table
#[derive(Debug, Default)]
pub struct AtomTable {
    atom_id_to_str: NoHashDashMap<Atom, EcoString>,
    str_to_atom_id: FxHashPapayaHashMap<EcoString, Atom>,
    next_atom_id: AtomicU32,
}

impl Clone for AtomTable {
    fn clone(&self) -> Self {
        Self {
            str_to_atom_id: self.str_to_atom_id.clone(),
            atom_id_to_str: self.atom_id_to_str.clone(),
            next_atom_id: AtomicU32::new(self.next_atom_id.load(Ordering::Relaxed)),
        }
    }
}

impl AtomTable {
    /// Create new atom table.
    #[inline]
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Create with capacity.
    #[inline]
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            str_to_atom_id: FxHashPapayaHashMap::with_capacity_and_hasher(capacity, FxBuildHasher),
            atom_id_to_str: NoHashDashMap::with_capacity_and_hasher(
                capacity,
                nohash_hasher::BuildNoHashHasher::default(),
            ),
            next_atom_id: AtomicU32::new(0),
        }
    }

    /// Intern a string. Fast-path is zero-allocation for existing strings.
    ///
    /// ```rust
    /// use atum::AtomTable;
    /// let tbl = AtomTable::new();
    /// let atom = tbl.intern("Hello, Sailor!");
    /// assert_eq!(tbl.lookup_ref(atom).as_ref(), "Hello, Sailor!");
    /// assert_eq!(tbl.intern("Hello, Sailor!"), atom);
    /// ```
    ///
    /// Returns an `Atom` unique for the interned string.
    #[inline(always)]
    pub fn intern(&self, s: &str) -> Atom {
        let g = self.str_to_atom_id.pin();

        // zero-alloc fast path
        if let Some(&id) = g.get(s) {
            return id;
        }

        self.intern_cold(s, &g)
    }

    /// Intern a string with a pre-pinned guard (for batch operations).
    ///
    /// This is more efficient when interning multiple strings in sequence
    /// as it avoids repeatedly pinning/unpinning the guard.
    #[inline]
    pub fn intern_with_guard(&self, s: &str, g: &StrToIdMapRef<'_, '_>) -> Atom {
        if let Some(&id) = g.get(s) {
            return id;
        }
        self.intern_cold(s, g)
    }

    /// Intern multiple strings in batch with optimal performance.
    ///
    /// This is the fastest way to intern many strings at once.
    #[inline]
    pub fn intern_batch<'a, I>(&self, strings: I) -> Vec<Atom>
    where
        I: IntoIterator<Item = &'a str>,
        I::IntoIter: ExactSizeIterator,
    {
        let iter = strings.into_iter();
        let mut result = Vec::with_capacity(iter.len());
        let g = self.str_to_atom_id.pin();

        for s in iter {
            result.push(self.intern_with_guard(s, &g));
        }

        result
    }

    #[cold]
    #[inline(never)]
    fn intern_cold(&self, s: &str, g: &StrToIdMapRef<'_, '_>) -> Atom {
        let key = EcoString::from(s);
        let key_ptr: *const EcoString = &key;

        *g.get_or_insert_with(key, || {
            let id = Atom(self.next_atom_id.fetch_add(1, Ordering::Relaxed));

            // Safety: key_ptr is valid because key is still in scope
            unsafe {
                self.atom_id_to_str.insert(id, EcoString::clone(&*key_ptr));
            }

            id
        })
    }

    /// Pin a guard for batch operations.
    ///
    /// Use this with `intern_with_guard` when interning many strings:
    /// ```rust
    /// use atum::AtomTable;
    /// let tbl = AtomTable::new();
    /// let guard = tbl.pin();
    /// let strings: &[&str] = &[
    ///     "unfortunately", "there's", "a",
    ///     "radio", "connected", "to", "my", "brain"
    /// ];
    /// for s in strings {
    ///     tbl.intern_with_guard(s, &guard);
    /// }
    /// ```
    #[inline]
    pub fn pin(&self) -> StrToIdMapRef<'_, '_> {
        self.str_to_atom_id.pin()
    }

    /// Lookup by id and return an owned `String` (allocates).
    #[inline]
    #[must_use]
    pub fn lookup_owned(&self, id: Atom) -> EcoString {
        EcoString::clone(&*self.atom_id_to_str.get(&id).unwrap())
    }

    /// Zero-allocation lookup returning a borrow that holds a read lock.
    #[inline]
    #[must_use]
    pub fn lookup_ref(&self, id: Atom) -> dashmap::mapref::one::Ref<'_, Atom, EcoString> {
        self.atom_id_to_str.get(&id).unwrap()
    }

    /// Check if a string is already interned without inserting it.
    ///
    /// Returns `Some(Atom)` if the string exists, `None` otherwise.
    #[inline]
    #[must_use]
    pub fn try_lookup(&self, s: &str) -> Option<Atom> {
        self.str_to_atom_id.pin().get(s).copied()
    }

    /// Check if a string is already interned using a pre-pinned guard.
    ///
    /// Faster than `try_lookup` when you already have a guard.
    #[inline]
    #[must_use]
    pub fn try_lookup_with_guard(&self, s: &str, g: &StrToIdMapRef<'_, '_>) -> Option<Atom> {
        g.get(s).copied()
    }

    /// Returns `Atom` if the string exists, panics otherwise.
    #[inline]
    #[must_use]
    pub fn lookup(&self, s: &str) -> Atom {
        self.try_lookup(s).unwrap()
    }

    /// Returns `Atom` if the string exists, panics otherwise.
    ///
    /// Faster than `lookup` when you already have a guard.
    #[inline]
    #[must_use]
    pub fn lookup_with_guard(&self, s: &str, g: &StrToIdMapRef<'_, '_>) -> Atom {
        self.try_lookup_with_guard(s, g).unwrap()
    }

    /// Number of interned strings.
    #[inline]
    #[must_use]
    pub fn len(&self) -> usize {
        self.str_to_atom_id.len()
    }

    /// Is empty
    #[inline]
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.str_to_atom_id.is_empty()
    }

    /// Clear all interned strings (resets ids)
    #[inline]
    pub fn clear(&self) {
        self.str_to_atom_id.pin().clear();
        self.atom_id_to_str.clear();
        self.next_atom_id.store(0, Ordering::Relaxed);
    }

    /// Iterate over all interned strings.
    ///
    /// Returns an iterator of `(Atom, EcoString)` pairs.
    /// The order is unspecified.
    #[inline]
    pub fn iter(&self) -> impl Iterator<Item = (Atom, EcoString)> + '_ {
        self.atom_id_to_str
            .iter()
            .map(|r| (*r.key(), EcoString::clone(&*r)))
    }
}
