use std::marker::PhantomData;
use std::ops::{
    Bound,
    RangeBounds,
};
use std::{
    fmt,
    mem,
    ptr,
    result,
    slice,
};

use libc::{
    c_uint,
    c_void,
    size_t,
    EINVAL,
};

use database::Database;
use error::{
    lmdb_result,
    Error,
    Result,
};
use ffi;
use flags::WriteFlags;
use transaction::Transaction;

/// An LMDB cursor.
pub trait Cursor<'txn> {
    /// Returns a raw pointer to the underlying LMDB cursor.
    ///
    /// The caller **must** ensure that the pointer is not used after the
    /// lifetime of the cursor.
    fn cursor(&self) -> *mut ffi::MDB_cursor;

    /// Retrieves a key/data pair from the cursor. Depending on the cursor op,
    /// the current key may be returned.
    fn get(&self, key: Option<&[u8]>, data: Option<&[u8]>, op: c_uint) -> Result<(Option<&'txn [u8]>, &'txn [u8])> {
        unsafe {
            let mut key_val = slice_to_val(key);
            let mut data_val = slice_to_val(data);
            let key_ptr = key_val.mv_data;
            lmdb_result(ffi::mdb_cursor_get(self.cursor(), &mut key_val, &mut data_val, op))?;
            let key_out = if key_ptr != key_val.mv_data {
                Some(val_to_slice(key_val))
            } else {
                None
            };
            let data_out = val_to_slice(data_val);
            Ok((key_out, data_out))
        }
    }

    /// Iterate over database items. The iterator will begin with item next
    /// after the cursor, and continue until the end of the database. For new
    /// cursors, the iterator will begin with the first item in the database.
    ///
    /// For databases with duplicate data items (`DatabaseFlags::DUP_SORT`), the
    /// duplicate data items of each key will be returned before moving on to
    /// the next key.
    fn iter(&mut self) -> Iter<'txn> {
        Iter::new(self.cursor(), ffi::MDB_NEXT, ffi::MDB_NEXT, Bound::Unbounded)
    }

    /// Iterate over database items backwards. The iterator will begin with item
    /// before the cursor, and continue until the start of the database. For new
    /// cursors, the iterator will begin with the last item in the database.
    ///
    /// For databases with duplicate data items (`DatabaseFlags::DUP_SORT`), the
    /// duplicate data items of each key will be returned before moving on to
    /// the next key.
    fn iter_backwards(&mut self) -> Iter<'txn> {
        Iter::new(self.cursor(), ffi::MDB_PREV, ffi::MDB_PREV, Bound::Unbounded)
    }

    /// Iterate over database items starting from the beginning of the database.
    ///
    /// For databases with duplicate data items (`DatabaseFlags::DUP_SORT`), the
    /// duplicate data items of each key will be returned before moving on to
    /// the next key.
    fn iter_start(&mut self) -> Iter<'txn> {
        Iter::new(self.cursor(), ffi::MDB_FIRST, ffi::MDB_NEXT, Bound::Unbounded)
    }

    /// Iterate backwards over database items starting from the end of the database.
    ///
    /// For databases with duplicate data items (`DatabaseFlags::DUP_SORT`), the
    /// duplicate data items of each key will be returned in reverse order before
    /// moving on to the next key.
    fn iter_end_backwards(&mut self) -> Iter<'txn> {
        Iter::new(self.cursor(), ffi::MDB_LAST, ffi::MDB_PREV, Bound::Unbounded)
    }

    /// Iterate over database items starting from the given key.
    ///
    /// For databases with duplicate data items (`DatabaseFlags::DUP_SORT`), the
    /// duplicate data items of each key will be returned before moving on to
    /// the next key.
    fn iter_from<K>(&mut self, key: K) -> Iter<'txn>
    where
        K: AsRef<[u8]>,
    {
        match self.get(Some(key.as_ref()), None, ffi::MDB_SET_RANGE) {
            Ok(_) | Err(Error::NotFound) => (),
            Err(error) => return Iter::Err(error),
        };
        Iter::new(self.cursor(), ffi::MDB_GET_CURRENT, ffi::MDB_NEXT, Bound::Unbounded)
    }

    /// Iterate over a `range` of database items.
    ///
    /// For databases with duplicate data items (`DatabaseFlags::DUP_SORT`), the
    /// duplicate data items of each key will be returned before moving on to
    /// the next key.
    fn iter_range<K, R>(&mut self, range: R) -> Iter<'txn>
    where
        K: AsRef<[u8]>,
        R: RangeBounds<K>,
    {
        let (start, first_op) = match range.start_bound() {
            Bound::Excluded(k) | Bound::Included(k) => (Some(k.as_ref()), ffi::MDB_SET_RANGE),
            Bound::Unbounded => (None, ffi::MDB_FIRST),
        };

        let end = match range.end_bound() {
            Bound::Excluded(k) => Bound::Excluded(k.as_ref().into()),
            Bound::Included(k) => Bound::Included(k.as_ref().into()),
            Bound::Unbounded => Bound::Unbounded,
        };

        let op = match self.get(start, None, first_op) {
            Ok((Some(key), _)) => match range.start_bound() {
                Bound::Excluded(k) if k.as_ref() == key => ffi::MDB_NEXT,
                _ => ffi::MDB_GET_CURRENT,
            },
            Ok((None, _)) | Err(Error::NotFound) => ffi::MDB_GET_CURRENT,
            Err(error) => return Iter::Err(error),
        };
        Iter::new(self.cursor(), op, ffi::MDB_NEXT, end)
    }

    /// Iterate over duplicate database items. The iterator will begin with the
    /// item next after the cursor, and continue until the end of the database.
    /// Each item will be returned as an iterator of its duplicates.
    fn iter_dup(&mut self) -> IterDup<'txn> {
        IterDup::new(self.cursor(), ffi::MDB_NEXT, Direction::Forwards)
    }

    /// Iterate backwards over duplicate database items. The iterator will begin with the
    /// item before the cursor, and continue until the start of the database.
    /// Each item will be returned as an iterator of its duplicates in reverse order.
    fn iter_dup_backwards(&mut self) -> IterDup<'txn> {
        IterDup::new(self.cursor(), ffi::MDB_PREV, Direction::Backwards)
    }

    /// Iterate over duplicate database items starting from the beginning of the
    /// database. Each item will be returned as an iterator of its duplicates.
    fn iter_dup_start(&mut self) -> IterDup<'txn> {
        IterDup::new(self.cursor(), ffi::MDB_FIRST, Direction::Forwards)
    }

    /// Iterate backwards over duplicate database items starting from the end of the
    /// database. Each item will be returned as an iterator of its duplicates in
    /// reverse order.
    fn iter_dup_end_backwards(&mut self) -> IterDup<'txn> {
        IterDup::new(self.cursor(), ffi::MDB_LAST, Direction::Backwards)
    }

    /// Iterate over duplicate items in the database starting from the given
    /// key. Each item will be returned as an iterator of its duplicates.
    fn iter_dup_from<K>(&mut self, key: K) -> IterDup<'txn>
    where
        K: AsRef<[u8]>,
    {
        match self.get(Some(key.as_ref()), None, ffi::MDB_SET_RANGE) {
            Ok(_) | Err(Error::NotFound) => (),
            Err(error) => return IterDup::Err(error),
        };
        IterDup::new(self.cursor(), ffi::MDB_GET_CURRENT, Direction::Forwards)
    }

    /// Iterate over the duplicates of the item in the database with the given key.
    fn iter_dup_of<K>(&mut self, key: K) -> Iter<'txn>
    where
        K: AsRef<[u8]>,
    {
        match self.get(Some(key.as_ref()), None, ffi::MDB_SET) {
            Ok(_) => (),
            Err(Error::NotFound) => {
                self.get(None, None, ffi::MDB_LAST).ok();
                return Iter::new(self.cursor(), ffi::MDB_NEXT, ffi::MDB_NEXT, Bound::Unbounded);
            },
            Err(error) => return Iter::Err(error),
        };
        Iter::new(self.cursor(), ffi::MDB_GET_CURRENT, ffi::MDB_NEXT_DUP, Bound::Unbounded)
    }
}

/// A read-only cursor for navigating the items within a database.
pub struct RoCursor<'txn> {
    cursor: *mut ffi::MDB_cursor,
    _marker: PhantomData<fn() -> &'txn ()>,
}

impl<'txn> Cursor<'txn> for RoCursor<'txn> {
    fn cursor(&self) -> *mut ffi::MDB_cursor {
        self.cursor
    }
}

impl<'txn> fmt::Debug for RoCursor<'txn> {
    fn fmt(&self, f: &mut fmt::Formatter) -> result::Result<(), fmt::Error> {
        f.debug_struct("RoCursor").finish()
    }
}

impl<'txn> Drop for RoCursor<'txn> {
    fn drop(&mut self) {
        unsafe { ffi::mdb_cursor_close(self.cursor) }
    }
}

impl<'txn> RoCursor<'txn> {
    /// Creates a new read-only cursor in the given database and transaction.
    /// Prefer using `Transaction::open_cursor`.
    pub(crate) fn new<T>(txn: &'txn T, db: Database) -> Result<RoCursor<'txn>>
    where
        T: Transaction,
    {
        let mut cursor: *mut ffi::MDB_cursor = ptr::null_mut();
        unsafe {
            lmdb_result(ffi::mdb_cursor_open(txn.txn(), db.dbi(), &mut cursor))?;
        }
        Ok(RoCursor {
            cursor,
            _marker: PhantomData,
        })
    }
}

/// A read-write cursor for navigating items within a database.
pub struct RwCursor<'txn> {
    cursor: *mut ffi::MDB_cursor,
    _marker: PhantomData<fn() -> &'txn ()>,
}

impl<'txn> Cursor<'txn> for RwCursor<'txn> {
    fn cursor(&self) -> *mut ffi::MDB_cursor {
        self.cursor
    }
}

impl<'txn> fmt::Debug for RwCursor<'txn> {
    fn fmt(&self, f: &mut fmt::Formatter) -> result::Result<(), fmt::Error> {
        f.debug_struct("RwCursor").finish()
    }
}

impl<'txn> Drop for RwCursor<'txn> {
    fn drop(&mut self) {
        unsafe { ffi::mdb_cursor_close(self.cursor) }
    }
}

impl<'txn> RwCursor<'txn> {
    /// Creates a new read-only cursor in the given database and transaction.
    /// Prefer using `RwTransaction::open_rw_cursor`.
    pub(crate) fn new<T>(txn: &'txn T, db: Database) -> Result<RwCursor<'txn>>
    where
        T: Transaction,
    {
        let mut cursor: *mut ffi::MDB_cursor = ptr::null_mut();
        unsafe {
            lmdb_result(ffi::mdb_cursor_open(txn.txn(), db.dbi(), &mut cursor))?;
        }
        Ok(RwCursor {
            cursor,
            _marker: PhantomData,
        })
    }

    /// Puts a key/data pair into the database. The cursor will be positioned at
    /// the new data item, or on failure usually near it.
    pub fn put<K, D>(&mut self, key: &K, data: &D, flags: WriteFlags) -> Result<()>
    where
        K: AsRef<[u8]>,
        D: AsRef<[u8]>,
    {
        let key = key.as_ref();
        let data = data.as_ref();
        let mut key_val: ffi::MDB_val = ffi::MDB_val {
            mv_size: key.len() as size_t,
            mv_data: key.as_ptr() as *mut c_void,
        };
        let mut data_val: ffi::MDB_val = ffi::MDB_val {
            mv_size: data.len() as size_t,
            mv_data: data.as_ptr() as *mut c_void,
        };
        unsafe { lmdb_result(ffi::mdb_cursor_put(self.cursor(), &mut key_val, &mut data_val, flags.bits())) }
    }

    /// Deletes the current key/data pair.
    ///
    /// ### Flags
    ///
    /// `WriteFlags::NO_DUP_DATA` may be used to delete all data items for the
    /// current key, if the database was opened with `DatabaseFlags::DUP_SORT`.
    pub fn del(&mut self, flags: WriteFlags) -> Result<()> {
        unsafe { lmdb_result(ffi::mdb_cursor_del(self.cursor(), flags.bits())) }
    }
}

unsafe fn slice_to_val(slice: Option<&[u8]>) -> ffi::MDB_val {
    match slice {
        Some(slice) => ffi::MDB_val {
            mv_size: slice.len() as size_t,
            mv_data: slice.as_ptr() as *mut c_void,
        },
        None => ffi::MDB_val {
            mv_size: 0,
            mv_data: ptr::null_mut(),
        },
    }
}

unsafe fn val_to_slice<'a>(val: ffi::MDB_val) -> &'a [u8] {
    slice::from_raw_parts(val.mv_data as *const u8, val.mv_size as usize)
}

/// An iterator over the key/value pairs in an LMDB database.
pub enum Iter<'txn> {
    /// An iterator that returns an error on every call to Iter.next().
    /// Cursor.iter*() creates an Iter of this type when LMDB returns an error
    /// on retrieval of a cursor.  Using this variant instead of returning
    /// an error makes Cursor.iter()* methods infallible, so consumers only
    /// need to check the result of Iter.next().
    Err(Error),

    /// An iterator that returns an Item on calls to Iter.next().
    /// The Item is a Result<(&'txn [u8], &'txn [u8])>, so this variant
    /// might still return an error, if retrieval of the key/value pair
    /// fails for some reason.
    Ok {
        /// The LMDB cursor with which to iterate.
        cursor: *mut ffi::MDB_cursor,

        /// The first operation to perform when the consumer calls Iter.next().
        op: c_uint,

        /// The next and subsequent operations to perform.
        next_op: c_uint,

        /// Upper bound.
        bound: Bound<Box<[u8]>>,

        /// A marker to ensure the iterator doesn't outlive the transaction.
        _marker: PhantomData<fn(&'txn ())>,
    },
}

impl<'txn> Iter<'txn> {
    /// Creates a new iterator backed by the given cursor.
    fn new<'t>(cursor: *mut ffi::MDB_cursor, op: c_uint, next_op: c_uint, bound: Bound<Box<[u8]>>) -> Iter<'t> {
        Iter::Ok {
            cursor,
            op,
            next_op,
            bound,
            _marker: PhantomData,
        }
    }
}

impl<'txn> fmt::Debug for Iter<'txn> {
    fn fmt(&self, f: &mut fmt::Formatter) -> result::Result<(), fmt::Error> {
        f.debug_struct("Iter").finish()
    }
}

impl<'txn> Iterator for Iter<'txn> {
    type Item = Result<(&'txn [u8], &'txn [u8])>;

    fn next(&mut self) -> Option<Result<(&'txn [u8], &'txn [u8])>> {
        match self {
            &mut Iter::Ok {
                cursor,
                ref mut op,
                next_op,
                ref bound,
                _marker,
            } => {
                let mut key = ffi::MDB_val {
                    mv_size: 0,
                    mv_data: ptr::null_mut(),
                };
                let mut data = ffi::MDB_val {
                    mv_size: 0,
                    mv_data: ptr::null_mut(),
                };
                let op = mem::replace(op, next_op);
                unsafe {
                    match ffi::mdb_cursor_get(cursor, &mut key, &mut data, op) {
                        ffi::MDB_SUCCESS => {
                            let key = val_to_slice(key);
                            let in_range = match bound {
                                Bound::Excluded(b) => key < b.as_ref(),
                                Bound::Included(b) => key <= b.as_ref(),
                                Bound::Unbounded => true,
                            };

                            if in_range {
                                Some(Ok((key, val_to_slice(data))))
                            } else {
                                None
                            }
                        },
                        // EINVAL can occur when the cursor was previously seeked to a non-existent value,
                        // e.g. iter_from with a key greater than all values in the database.
                        ffi::MDB_NOTFOUND | EINVAL => None,
                        error => Some(Err(Error::from_err_code(error))),
                    }
                }
            },
            &mut Iter::Err(err) => Some(Err(err)),
        }
    }
}

pub enum Direction {
    Forwards,
    Backwards,
}

/// An iterator over the keys and duplicate values in an LMDB database.
///
/// The yielded items of the iterator are themselves iterators over the duplicate values for a
/// specific key.
pub enum IterDup<'txn> {
    /// An iterator that returns an error on every call to Iter.next().
    /// Cursor.iter*() creates an Iter of this type when LMDB returns an error
    /// on retrieval of a cursor.  Using this variant instead of returning
    /// an error makes Cursor.iter()* methods infallible, so consumers only
    /// need to check the result of Iter.next().
    Err(Error),

    /// An iterator that returns an Item on calls to Iter.next().
    /// The Item is a Result<(&'txn [u8], &'txn [u8])>, so this variant
    /// might still return an error, if retrieval of the key/value pair
    /// fails for some reason.
    Ok {
        /// The LMDB cursor with which to iterate.
        cursor: *mut ffi::MDB_cursor,

        /// The first operation to perform when the consumer calls Iter.next().
        op: c_uint,

        /// Forwards or backwards iteration direction
        direction: Direction,

        /// A marker to ensure the iterator doesn't outlive the transaction.
        _marker: PhantomData<fn(&'txn ())>,
    },
}

impl<'txn> IterDup<'txn> {
    /// Creates a new iterator backed by the given cursor.
    fn new<'t>(cursor: *mut ffi::MDB_cursor, op: c_uint, direction: Direction) -> IterDup<'t> {
        IterDup::Ok {
            cursor,
            op,
            direction,
            _marker: PhantomData,
        }
    }
}

impl<'txn> fmt::Debug for IterDup<'txn> {
    fn fmt(&self, f: &mut fmt::Formatter) -> result::Result<(), fmt::Error> {
        f.debug_struct("IterDup").finish()
    }
}

impl<'txn> Iterator for IterDup<'txn> {
    type Item = Iter<'txn>;

    fn next(&mut self) -> Option<Iter<'txn>> {
        match self {
            &mut IterDup::Ok {
                cursor,
                ref mut op,
                ref direction,
                _marker,
            } => {
                let mut key = ffi::MDB_val {
                    mv_size: 0,
                    mv_data: ptr::null_mut(),
                };
                let mut data = ffi::MDB_val {
                    mv_size: 0,
                    mv_data: ptr::null_mut(),
                };
                let op = mem::replace(
                    op,
                    match direction {
                        Direction::Forwards => ffi::MDB_NEXT_NODUP,
                        Direction::Backwards => ffi::MDB_PREV_NODUP,
                    },
                );
                let err_code = unsafe { ffi::mdb_cursor_get(cursor, &mut key, &mut data, op) };

                if err_code == ffi::MDB_SUCCESS {
                    Some(Iter::new(
                        cursor,
                        ffi::MDB_GET_CURRENT,
                        match direction {
                            Direction::Forwards => ffi::MDB_NEXT_DUP,
                            Direction::Backwards => ffi::MDB_PREV_DUP,
                        },
                        Bound::Unbounded,
                    ))
                } else {
                    None
                }
            },
            &mut IterDup::Err(err) => Some(Iter::Err(err)),
        }
    }
}

#[cfg(test)]
mod test {
    use tempdir::TempDir;

    use super::*;
    use environment::*;
    use ffi::*;
    use flags::*;

    #[test]
    fn test_get() {
        let dir = TempDir::new("test").unwrap();
        let env = Environment::new().open(dir.path()).unwrap();
        let db = env.open_db(None).unwrap();

        let mut txn = env.begin_rw_txn().unwrap();
        txn.put(db, b"key1", b"val1", WriteFlags::empty()).unwrap();
        txn.put(db, b"key2", b"val2", WriteFlags::empty()).unwrap();
        txn.put(db, b"key3", b"val3", WriteFlags::empty()).unwrap();

        let cursor = txn.open_ro_cursor(db).unwrap();
        assert_eq!((Some(&b"key1"[..]), &b"val1"[..]), cursor.get(None, None, MDB_FIRST).unwrap());
        assert_eq!((Some(&b"key1"[..]), &b"val1"[..]), cursor.get(None, None, MDB_GET_CURRENT).unwrap());
        assert_eq!((Some(&b"key2"[..]), &b"val2"[..]), cursor.get(None, None, MDB_NEXT).unwrap());
        assert_eq!((Some(&b"key1"[..]), &b"val1"[..]), cursor.get(None, None, MDB_PREV).unwrap());
        assert_eq!((Some(&b"key3"[..]), &b"val3"[..]), cursor.get(None, None, MDB_LAST).unwrap());
        assert_eq!((None, &b"val2"[..]), cursor.get(Some(b"key2"), None, MDB_SET).unwrap());
        assert_eq!((Some(&b"key3"[..]), &b"val3"[..]), cursor.get(Some(&b"key3"[..]), None, MDB_SET_KEY).unwrap());
        assert_eq!((Some(&b"key3"[..]), &b"val3"[..]), cursor.get(Some(&b"key2\0"[..]), None, MDB_SET_RANGE).unwrap());
    }

    #[test]
    fn test_get_dup() {
        let dir = TempDir::new("test").unwrap();
        let env = Environment::new().open(dir.path()).unwrap();
        let db = env.create_db(None, DatabaseFlags::DUP_SORT).unwrap();

        let mut txn = env.begin_rw_txn().unwrap();
        txn.put(db, b"key1", b"val1", WriteFlags::empty()).unwrap();
        txn.put(db, b"key1", b"val2", WriteFlags::empty()).unwrap();
        txn.put(db, b"key1", b"val3", WriteFlags::empty()).unwrap();
        txn.put(db, b"key2", b"val1", WriteFlags::empty()).unwrap();
        txn.put(db, b"key2", b"val2", WriteFlags::empty()).unwrap();
        txn.put(db, b"key2", b"val3", WriteFlags::empty()).unwrap();

        let cursor = txn.open_ro_cursor(db).unwrap();
        assert_eq!((Some(&b"key1"[..]), &b"val1"[..]), cursor.get(None, None, MDB_FIRST).unwrap());
        assert_eq!((None, &b"val1"[..]), cursor.get(None, None, MDB_FIRST_DUP).unwrap());
        assert_eq!((Some(&b"key1"[..]), &b"val1"[..]), cursor.get(None, None, MDB_GET_CURRENT).unwrap());
        assert_eq!((Some(&b"key2"[..]), &b"val1"[..]), cursor.get(None, None, MDB_NEXT_NODUP).unwrap());
        assert_eq!((Some(&b"key2"[..]), &b"val2"[..]), cursor.get(None, None, MDB_NEXT_DUP).unwrap());
        assert_eq!((Some(&b"key2"[..]), &b"val3"[..]), cursor.get(None, None, MDB_NEXT_DUP).unwrap());
        assert!(cursor.get(None, None, MDB_NEXT_DUP).is_err());
        assert_eq!((Some(&b"key2"[..]), &b"val2"[..]), cursor.get(None, None, MDB_PREV_DUP).unwrap());
        assert_eq!((None, &b"val3"[..]), cursor.get(None, None, MDB_LAST_DUP).unwrap());
        assert_eq!((Some(&b"key1"[..]), &b"val3"[..]), cursor.get(None, None, MDB_PREV_NODUP).unwrap());
        assert_eq!((None, &b"val1"[..]), cursor.get(Some(&b"key1"[..]), None, MDB_SET).unwrap());
        assert_eq!((Some(&b"key2"[..]), &b"val1"[..]), cursor.get(Some(&b"key2"[..]), None, MDB_SET_KEY).unwrap());
        assert_eq!((Some(&b"key2"[..]), &b"val1"[..]), cursor.get(Some(&b"key1\0"[..]), None, MDB_SET_RANGE).unwrap());
        assert_eq!((None, &b"val3"[..]), cursor.get(Some(&b"key1"[..]), Some(&b"val3"[..]), MDB_GET_BOTH).unwrap());
        assert_eq!(
            (None, &b"val1"[..]),
            cursor.get(Some(&b"key2"[..]), Some(&b"val"[..]), MDB_GET_BOTH_RANGE).unwrap()
        );
    }

    #[test]
    fn test_get_dupfixed() {
        let dir = TempDir::new("test").unwrap();
        let env = Environment::new().open(dir.path()).unwrap();
        let db = env.create_db(None, DatabaseFlags::DUP_SORT | DatabaseFlags::DUP_FIXED).unwrap();

        let mut txn = env.begin_rw_txn().unwrap();
        txn.put(db, b"key1", b"val1", WriteFlags::empty()).unwrap();
        txn.put(db, b"key1", b"val2", WriteFlags::empty()).unwrap();
        txn.put(db, b"key1", b"val3", WriteFlags::empty()).unwrap();
        txn.put(db, b"key2", b"val4", WriteFlags::empty()).unwrap();
        txn.put(db, b"key2", b"val5", WriteFlags::empty()).unwrap();
        txn.put(db, b"key2", b"val6", WriteFlags::empty()).unwrap();

        let cursor = txn.open_ro_cursor(db).unwrap();
        assert_eq!((Some(&b"key1"[..]), &b"val1"[..]), cursor.get(None, None, MDB_FIRST).unwrap());
        assert_eq!((None, &b"val1val2val3"[..]), cursor.get(None, None, MDB_GET_MULTIPLE).unwrap());
        assert!(cursor.get(None, None, MDB_NEXT_MULTIPLE).is_err());
    }

    #[test]
    fn test_iter() {
        let dir = TempDir::new("test").unwrap();
        let env = Environment::new().open(dir.path()).unwrap();
        let db = env.open_db(None).unwrap();

        let items: Vec<(&[u8], &[u8])> =
            vec![(b"key1", b"val1"), (b"key2", b"val2"), (b"key3", b"val3"), (b"key5", b"val5")];

        {
            let mut txn = env.begin_rw_txn().unwrap();
            for &(ref key, ref data) in &items {
                txn.put(db, key, data, WriteFlags::empty()).unwrap();
            }
            txn.commit().unwrap();
        }

        let txn = env.begin_ro_txn().unwrap();
        let mut cursor = txn.open_ro_cursor(db).unwrap();

        // Because Result implements FromIterator, we can collect the iterator
        // of items of type Result<_, E> into a Result<Vec<_, E>> by specifying
        // the collection type via the turbofish syntax.
        assert_eq!(items, cursor.iter().collect::<Result<Vec<_>>>().unwrap());

        // Alternately, we can collect it into an appropriately typed variable.
        let retr: Result<Vec<_>> = cursor.iter_start().collect();
        assert_eq!(items, retr.unwrap());

        cursor.get(Some(b"key2"), None, MDB_SET).unwrap();
        assert_eq!(
            items.clone().into_iter().skip(2).collect::<Vec<_>>(),
            cursor.iter().collect::<Result<Vec<_>>>().unwrap()
        );

        assert_eq!(items, cursor.iter_start().collect::<Result<Vec<_>>>().unwrap());

        assert_eq!(
            items.clone().into_iter().skip(1).collect::<Vec<_>>(),
            cursor.iter_from(b"key2").collect::<Result<Vec<_>>>().unwrap()
        );

        assert_eq!(
            items.clone().into_iter().skip(3).collect::<Vec<_>>(),
            cursor.iter_from(b"key4").collect::<Result<Vec<_>>>().unwrap()
        );

        assert_eq!(
            vec!().into_iter().collect::<Vec<(&[u8], &[u8])>>(),
            cursor.iter_from(b"key6").collect::<Result<Vec<_>>>().unwrap()
        );

        // backwards iterators
        let reverse_items = items.clone().into_iter().rev().collect::<Vec<(&[u8], &[u8])>>();
        let mut cursor = txn.open_ro_cursor(db).unwrap();

        assert_eq!(reverse_items, cursor.iter_backwards().collect::<Result<Vec<_>>>().unwrap());

        cursor.get(Some(b"key3"), None, MDB_SET).unwrap();
        assert_eq!(
            reverse_items.clone().into_iter().skip(2).collect::<Vec<_>>(),
            cursor.iter_backwards().collect::<Result<Vec<_>>>().unwrap()
        );

        // ping pong
        assert_eq!(reverse_items, cursor.iter_end_backwards().collect::<Result<Vec<_>>>().unwrap());
        assert_eq!(
            items.clone().into_iter().skip(1).collect::<Vec<_>>(),
            cursor.iter().collect::<Result<Vec<_>>>().unwrap()
        );
        assert_eq!(
            reverse_items.clone().into_iter().skip(1).collect::<Vec<_>>(),
            cursor.iter_backwards().collect::<Result<Vec<_>>>().unwrap()
        );
    }

    #[test]
    fn test_iter_empty_database() {
        let dir = TempDir::new("test").unwrap();
        let env = Environment::new().open(dir.path()).unwrap();
        let db = env.open_db(None).unwrap();
        let txn = env.begin_ro_txn().unwrap();
        let mut cursor = txn.open_ro_cursor(db).unwrap();

        assert_eq!(0, cursor.iter().count());
        assert_eq!(0, cursor.iter_start().count());
        assert_eq!(0, cursor.iter_from(b"foo").count());
    }

    #[test]
    fn test_iter_empty_dup_database() {
        let dir = TempDir::new("test").unwrap();
        let env = Environment::new().open(dir.path()).unwrap();
        let db = env.create_db(None, DatabaseFlags::DUP_SORT).unwrap();
        let txn = env.begin_ro_txn().unwrap();
        let mut cursor = txn.open_ro_cursor(db).unwrap();

        assert_eq!(0, cursor.iter().count());
        assert_eq!(0, cursor.iter_start().count());
        assert_eq!(0, cursor.iter_from(b"foo").count());
        assert_eq!(0, cursor.iter_dup().count());
        assert_eq!(0, cursor.iter_dup_start().count());
        assert_eq!(0, cursor.iter_dup_from(b"foo").count());
        assert_eq!(0, cursor.iter_dup_of(b"foo").count());
    }

    #[test]
    fn test_iter_dup() {
        let dir = TempDir::new("test").unwrap();
        let env = Environment::new().open(dir.path()).unwrap();
        let db = env.create_db(None, DatabaseFlags::DUP_SORT).unwrap();

        let items: Vec<(&[u8], &[u8])> = vec![
            (b"a", b"1"),
            (b"a", b"2"),
            (b"a", b"3"),
            (b"b", b"1"),
            (b"b", b"2"),
            (b"b", b"3"),
            (b"c", b"1"),
            (b"c", b"2"),
            (b"c", b"3"),
            (b"e", b"1"),
            (b"e", b"2"),
            (b"e", b"3"),
        ];

        {
            let mut txn = env.begin_rw_txn().unwrap();
            for &(ref key, ref data) in &items {
                txn.put(db, key, data, WriteFlags::empty()).unwrap();
            }
            txn.commit().unwrap();
        }

        let txn = env.begin_ro_txn().unwrap();
        let mut cursor = txn.open_ro_cursor(db).unwrap();
        assert_eq!(items, cursor.iter_dup().flatten().collect::<Result<Vec<_>>>().unwrap());

        cursor.get(Some(b"b"), None, MDB_SET).unwrap();
        assert_eq!(
            items.clone().into_iter().skip(4).collect::<Vec<(&[u8], &[u8])>>(),
            cursor.iter_dup().flatten().collect::<Result<Vec<_>>>().unwrap()
        );

        assert_eq!(items, cursor.iter_dup_start().flatten().collect::<Result<Vec<(&[u8], &[u8])>>>().unwrap());

        assert_eq!(
            items.clone().into_iter().skip(3).collect::<Vec<(&[u8], &[u8])>>(),
            cursor.iter_dup_from(b"b").flatten().collect::<Result<Vec<_>>>().unwrap()
        );

        assert_eq!(
            items.clone().into_iter().skip(3).collect::<Vec<(&[u8], &[u8])>>(),
            cursor.iter_dup_from(b"ab").flatten().collect::<Result<Vec<_>>>().unwrap()
        );

        assert_eq!(
            items.clone().into_iter().skip(9).collect::<Vec<(&[u8], &[u8])>>(),
            cursor.iter_dup_from(b"d").flatten().collect::<Result<Vec<_>>>().unwrap()
        );

        assert_eq!(
            vec!().into_iter().collect::<Vec<(&[u8], &[u8])>>(),
            cursor.iter_dup_from(b"f").flatten().collect::<Result<Vec<_>>>().unwrap()
        );

        assert_eq!(
            items.clone().into_iter().skip(3).take(3).collect::<Vec<(&[u8], &[u8])>>(),
            cursor.iter_dup_of(b"b").collect::<Result<Vec<_>>>().unwrap()
        );

        assert_eq!(0, cursor.iter_dup_of(b"foo").count());

        // backwards iterators
        let reverse_items = items.clone().into_iter().rev().collect::<Vec<(&[u8], &[u8])>>();
        let mut cursor = txn.open_ro_cursor(db).unwrap();

        assert_eq!(reverse_items, cursor.iter_dup_backwards().flatten().collect::<Result<Vec<_>>>().unwrap());

        cursor.get(Some(b"c"), None, MDB_SET).unwrap();
        assert_eq!(
            reverse_items.clone().into_iter().skip(6).collect::<Vec<(&[u8], &[u8])>>(),
            cursor.iter_dup_backwards().flatten().collect::<Result<Vec<_>>>().unwrap()
        );

        assert_eq!(
            reverse_items,
            cursor.iter_dup_end_backwards().flatten().collect::<Result<Vec<(&[u8], &[u8])>>>().unwrap()
        );

        // ping pong
        assert_eq!(reverse_items, cursor.iter_dup_end_backwards().flatten().collect::<Result<Vec<_>>>().unwrap());
        assert_eq!(
            items.clone().into_iter().skip(1).collect::<Vec<_>>(),
            cursor.iter_dup().flatten().collect::<Result<Vec<_>>>().unwrap()
        );
        assert_eq!(
            reverse_items.clone().into_iter().skip(1).collect::<Vec<_>>(),
            cursor.iter_dup_backwards().flatten().collect::<Result<Vec<_>>>().unwrap()
        );
    }

    #[test]
    fn test_iter_del_get() {
        let dir = TempDir::new("test").unwrap();
        let env = Environment::new().open(dir.path()).unwrap();
        let db = env.create_db(None, DatabaseFlags::DUP_SORT).unwrap();

        let items: Vec<(&[u8], &[u8])> = vec![(b"a", b"1"), (b"b", b"2")];
        let r: Vec<(&[u8], &[u8])> = Vec::new();
        {
            let txn = env.begin_ro_txn().unwrap();
            let mut cursor = txn.open_ro_cursor(db).unwrap();
            assert_eq!(r, cursor.iter_dup_of(b"a").collect::<Result<Vec<_>>>().unwrap());
        }

        {
            let mut txn = env.begin_rw_txn().unwrap();
            for &(ref key, ref data) in &items {
                txn.put(db, key, data, WriteFlags::empty()).unwrap();
            }
            txn.commit().unwrap();
        }

        let mut txn = env.begin_rw_txn().unwrap();
        let mut cursor = txn.open_rw_cursor(db).unwrap();
        assert_eq!(items, cursor.iter_dup().flat_map(|x| x).collect::<Result<Vec<_>>>().unwrap());

        assert_eq!(
            items.clone().into_iter().take(1).collect::<Vec<(&[u8], &[u8])>>(),
            cursor.iter_dup_of(b"a").collect::<Result<Vec<_>>>().unwrap()
        );

        assert_eq!((None, &b"1"[..]), cursor.get(Some(b"a"), Some(b"1"), MDB_SET).unwrap());

        cursor.del(WriteFlags::empty()).unwrap();

        assert_eq!(r, cursor.iter_dup_of(b"a").collect::<Result<Vec<_>>>().unwrap());
    }

    #[test]
    fn test_put_del() {
        let dir = TempDir::new("test").unwrap();
        let env = Environment::new().open(dir.path()).unwrap();
        let db = env.open_db(None).unwrap();

        let mut txn = env.begin_rw_txn().unwrap();
        let mut cursor = txn.open_rw_cursor(db).unwrap();

        cursor.put(b"key1", b"val1", WriteFlags::empty()).unwrap();
        cursor.put(b"key2", b"val2", WriteFlags::empty()).unwrap();
        cursor.put(b"key3", b"val3", WriteFlags::empty()).unwrap();

        assert_eq!((Some(&b"key3"[..]), &b"val3"[..]), cursor.get(None, None, MDB_GET_CURRENT).unwrap());

        cursor.del(WriteFlags::empty()).unwrap();
        assert_eq!((Some(&b"key2"[..]), &b"val2"[..]), cursor.get(None, None, MDB_LAST).unwrap());
    }

    #[test]
    fn test_iter_range() {
        let dir = TempDir::new("test").unwrap();
        let env = Environment::new().open(dir.path()).unwrap();
        let db = env.open_db(None).unwrap();

        let items: Vec<(&[u8], &[u8])> =
            vec![(b"key1", b"val1"), (b"key2", b"val2"), (b"key3", b"val3"), (b"key5", b"val5")];

        let mut txn = env.begin_rw_txn().unwrap();
        for (key, data) in &items {
            txn.put(db, key, data, WriteFlags::empty()).unwrap();
        }
        txn.commit().unwrap();

        let txn = env.begin_ro_txn().unwrap();
        let mut cursor = txn.open_ro_cursor(db).unwrap();

        // Range bounded inclusively below and above
        assert_eq!(
            items.clone().into_iter().skip(1).take(2).collect::<Vec<_>>(),
            cursor.iter_range(&b"key2"[..]..=b"key3").collect::<Result<Vec<_>>>().unwrap()
        );

        // Range bounded inclusively below and exclusively above
        assert_eq!(
            items.clone().into_iter().skip(1).take(1).collect::<Vec<_>>(),
            cursor.iter_range(&b"key2"[..]..b"key3").collect::<Result<Vec<_>>>().unwrap()
        );

        // Range bounded exclusively below and above
        assert_eq!(
            items.clone().into_iter().skip(4).collect::<Vec<_>>(),
            cursor
                .iter_range::<&[u8; 4], _>((Bound::Excluded(b"key2"), Bound::Excluded(b"key3")))
                .collect::<Result<Vec<_>>>()
                .unwrap()
        );

        // Range bounded exclusively below and inclusively above
        assert_eq!(
            items.clone().into_iter().skip(2).take(1).collect::<Vec<_>>(),
            cursor
                .iter_range::<&[u8; 4], _>((Bound::Excluded(b"key2"), Bound::Included(b"key3")))
                .collect::<Result<Vec<_>>>()
                .unwrap()
        );

        // Range only bounded exclusively below
        assert_eq!(
            items.clone().into_iter().skip(2).collect::<Vec<_>>(),
            cursor
                .iter_range::<&[u8; 4], _>((Bound::Excluded(b"key2"), Bound::Unbounded))
                .collect::<Result<Vec<_>>>()
                .unwrap()
        );

        // Range only bounded inclusively below
        assert_eq!(
            items.clone().into_iter().skip(1).collect::<Vec<_>>(),
            cursor.iter_range(&b"key2"[..]..).collect::<Result<Vec<_>>>().unwrap()
        );

        // Range only bounded exclusively above
        assert_eq!(
            items.clone().into_iter().take(3).collect::<Vec<_>>(),
            cursor.iter_range(..&b"key5"[..]).collect::<Result<Vec<_>>>().unwrap()
        );

        // Range only bounded inclusively above
        assert_eq!(
            items.clone().into_iter().take(3).collect::<Vec<_>>(),
            cursor.iter_range(..=&b"key3"[..]).collect::<Result<Vec<_>>>().unwrap()
        );

        // Unbounded range
        assert_eq!(
            items.clone().into_iter().collect::<Vec<_>>(),
            cursor.iter_range::<&[u8], _>(..).collect::<Result<Vec<_>>>().unwrap()
        );
    }
}
