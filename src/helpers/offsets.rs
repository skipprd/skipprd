use libc::sleep;
use sled;
use Result;

use crate::helpers::configuration::Config;
use crate::helpers::offsets::OffsetsError::VacuumError;
use crate::helpers::Helpers;
use crate::METRICS;
use serde_derive::{Deserialize, Serialize};
use sled::{IVec, Mode};
use thiserror::Error;
use tracing::{error, info};
use {
    byteorder::{BigEndian, LittleEndian},
    zerocopy::{byteorder::U64, AsBytes, FromBytes, LayoutVerified, Unaligned, U16},
};

pub const SLED_NAME: &str = "db";

// We use `BigEndian` for key types because
// they preserve lexicographic ordering,
// which is nice if we ever want to iterate
// over our items in order. We use the
// `U64` type from zerocopy because it
// does not have alignment requirements.
// sled does not guarantee any particular
// value alignment as of now.
#[derive(Debug, Clone, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[repr(C)]
pub struct OffsetKey {
    pub(crate) namespace: String,
    pub(crate) partition: String,
}

#[derive(Debug)]
pub enum OffsetTypes {
    Filesize,
    Position,
    Closed,
}

// We use `LittleEndian` for values because
// it's possibly cheaper, but the difference
// isn't likely to be measurable, so honestly
// use whatever you want for values.
#[derive(FromBytes, AsBytes, Unaligned, Debug)]
#[repr(C)]
pub struct OffsetValue {
    pub(crate) filesize: U64<LittleEndian>,
    pub(crate) line: U64<LittleEndian>,
    pub(crate) closed: U64<LittleEndian>, // we store bool here
}

#[allow(dead_code)]
pub struct Offset {
    pub(crate) value: OffsetValue,
    pub(crate) key: OffsetKey,
}

#[derive(Clone)]
pub struct Offsets {
    /// The Key-Value store that contains all offset data.
    /// Resources can be found using their Subject.
    /// Try not to use this directly, but use the Trees.
    #[allow(dead_code)]
    db: sled::Db,
    pub(crate) tree: sled::Tree,
}

// #[derive(Debug, Error)]
// #[error("Failed opening offset DB at this location: {0}. Is another instance of Skippr already ingesting this pipeline?")]
// struct AlreadyOpenError(String);

#[derive(Debug, Error)]
pub enum OffsetsError {
    #[error("Failed opening offset DB at this location: {0}. Is another instance of Skippr already ingesting this pipeline?")]
    AlreadyOpenError(String),
    #[error("Failed vacuuming offsets database, Error: {0}")]
    VacuumError(sled::Error),
}

impl Offsets {
    pub fn init() -> Result<Offsets, OffsetsError> {
        // match Self::vacuum() { // requires a full scan of table which is expensive on EFS since we're opting to keep all offsets to support replays
        //     Ok(size) => {}
        //     Err(err) => {
        //         println!("Failed vacuuming offsets database, Error: {:?}", err);
        //         unsafe { exit(1); }
        //     }
        // }

        let db_path = format!("{}/{}", Config::get_data_dir(), SLED_NAME);
        let db = match sled::open(&db_path) {
            // open in high-throughput mode
            Ok(db) => db,
            Err(_err) => {
                return Err(OffsetsError::AlreadyOpenError(db_path));
            }
        };
        let tree = db.open_tree("offsets").expect("Could not open offset tree");

        let total_size_bytes = db.size_on_disk().unwrap_or_else(|err| {
            error!("Failed getting size of offsets DB, Error: {:?}", err);
            0
        });

        info!(
            "Offset DB size: {}",
            Helpers::human_readable_size(total_size_bytes)
        );

        let mut metrics_lock = METRICS.write();
        metrics_lock.offset_db_size = total_size_bytes;

        // let names: Vec<String> = db
        //     .tree_names()
        //     .iter()
        //     .filter_map(|name| match std::str::from_utf8(name) {
        //         Ok(value) => Some(value.to_string()),
        //         Err(e) => {
        //             println!("Sled name={name:?} caused error: {e:?}");
        //             None
        //         }
        //     })
        //     .collect();
        //
        // println!("Tree names: {:?}", names);

        // Iterate over all key-value pairs and print them
        // for kv in tree.iter() {
        //     let mut key = kv.unwrap().0;
        //     let mut value = match tree.get(&key) {
        //         Ok(val) => match val {
        //             Some(val) => val,
        //             None => IVec::from("none")
        //         },
        //         Err(err) => {
        //             println!("Error: {}", err);
        //             IVec::from("none")
        //         }
        //     };
        //
        //     // let key = std::str::from_utf8(&key);
        //
        //     let layout: LayoutVerified<&mut [u8], Value> =
        //         LayoutVerified::new_unaligned(&mut *value)
        //             .expect("bytes do not fit schema");
        //     let value: &mut Value = layout.into_mut();
        //
        //     // let layout: LayoutVerified<&mut [u8], Key> =
        //     //     LayoutVerified::new_unaligned(&mut *key)
        //     //         .expect("bytes do not fit schema");
        //     // let key: &mut Key = layout.into_mut();
        //
        //     // let key: Key = key.as_bytes();
        //     // let key: &mut Key = layout.into_mut();
        //
        //     println!("Key: {:?}, Value: {:?}", key, value);
        // }

        let store = Offsets { db, tree };

        Ok(store)
    }

    // Sled remove() currently sets the value to None, and maintains the key in the tree.
    // Since the key is the largest part of the data, we need to purge keys with None values
    // periodically to save space.
    #[allow(dead_code)]
    fn vacuum() -> Result<u64, OffsetsError> {
        let db_path = format!("{}/{}", Config::get_data_dir(), SLED_NAME);
        // // Ensure database isn't already open before we start operating
        // let db = match sled::Config::default()
        //     .path(&db_path)
        //     .mode(Mode::LowSpace)// open in low space mode to encourage GC
        //     .open() {
        //     Ok(db) => {db}
        //     Err(_) => {
        //         return Err(OffsetsError::AlreadyOpenError(db_path));
        //     }
        // };

        // // rollback any previous vacuum that was interrupted
        // match Self::rollback_vacuum() {
        //     Ok(true) => {
        //         println!("Rolled back previous interrupted offsets db vacuum");
        //     },
        //     Ok(false) => {},
        //     Err(err) => {
        //         println!("Failed rolling back previous interrupted offsets db vacuum, Error: {:?}", err);
        //         unsafe { exit(1) }
        //     }
        // }

        // drop(db);

        // Rename database file to a temporary file
        // let temp_db_path = format!("{}/{}.tmp", Config::get_data_dir(), SLED_NAME);
        //
        // if std::fs::metadata(&db_path).is_err() {
        //     return Ok(0);
        // }
        // std::fs::rename(&db_path, &temp_db_path).unwrap();
        //
        // // open old db
        // let old_db = match sled::Config::default()
        //     .path(&temp_db_path)
        //     .mode(Mode::LowSpace)
        //     .open() {
        //     Ok(db) => {db}
        //     Err(err) => {
        //         return Err(OffsetsError::AlreadyOpenError(temp_db_path));
        //     }
        // };
        // let old_tree = old_db.open_tree("offsets").expect("Could not open offset tree");
        //
        // println!("Vacuuming offsets database of size: {}", Helpers::human_readable_size(old_db.size_on_disk().unwrap()));

        // write all keys with values to a new database
        let db = match sled::Config::default()
            .path(&db_path)
            .mode(Mode::LowSpace) // open in low space mode to encourage GC
            .open()
        {
            Ok(db) => db,
            Err(_) => {
                return Err(OffsetsError::AlreadyOpenError(db_path));
            }
        };
        let tree = db.open_tree("offsets").expect("Could not open offset tree");

        println!(
            "Vacuuming offsets database of size: {}",
            Helpers::human_readable_size(db.size_on_disk().unwrap())
        );
        let key_count = db.len();

        let mut i = 0;
        let mut count = 0;

        let pause_modus = key_count / 60; // 60 sec total pause for sled gc (plus insert time)
        let pause_modus = pause_modus.max(1000);

        for kv in db.iter() {
            let key = kv.unwrap().0;
            let op = match db.get(&key) {
                Ok(val) => match val {
                    Some(val) =>
                    // Ok(Some(val)),
                    {
                        match tree.insert(&key, &val) {
                            Ok(val) => Ok(val),
                            Err(err) => Err(sled::Error::ReportableBug(format!(
                                "Failed inserting key into new tree, Error: {:?}",
                                err
                            ))),
                        }
                    }
                    None => {
                        // println!("Removing key: {:?}", key);
                        tree.remove(&key).unwrap_or_default();
                        count += 1;
                        Ok(None)
                    }
                },
                Err(err) => Err(sled::Error::ReportableBug(format!(
                    "Failed getting key from old tree, Error: {:?}",
                    err
                ))),
            };

            i += 1;

            if i % pause_modus == 0 {
                tree.flush().unwrap();
                // after experimentation, sled does better job of GC with smaller writes. So we'll do it more often with shorter sleep
                unsafe {
                    sleep(1);
                }

                let new_size = db.size_on_disk().unwrap_or_else(|err| {
                    println!("Failed getting size of new offsets DB, Error: {:?}", err);
                    0
                });

                println!(
                    "Vacuumed {} offsets from db, evaluated {}/{} keys, size {}",
                    count,
                    i,
                    key_count,
                    Helpers::human_readable_size(new_size)
                );

                count = 0;
            }

            if let Err(err) = op {
                // rollback
                drop(tree);
                drop(db);
                // drop(old_tree);
                // drop(old_db);
                // std::fs::remove_dir_all(&db_path).unwrap();
                // std::fs::rename(&temp_db_path, &db_path).unwrap();

                return Err(VacuumError(err));
            }
        }

        unsafe {
            sleep(5);
        }

        let new_size = db.size_on_disk().unwrap_or_else(|err| {
            println!("Failed getting size of new offsets DB, Error: {:?}", err);
            0
        });

        println!(
            "Vacuumed offsets database, size: {}",
            Helpers::human_readable_size(new_size)
        );

        // delete old file
        drop(tree);
        drop(db);
        // drop(old_tree);
        // drop(old_db);

        // std::fs::remove_dir_all(&temp_db_path).unwrap();

        Ok(new_size)
    }

    #[allow(dead_code)]
    fn rollback_vacuum() -> Result<bool, OffsetsError> {
        let db_path = format!("{}/{}", Config::get_data_dir(), SLED_NAME);
        let temp_db_path = format!("{}/{}.tmp", Config::get_data_dir(), SLED_NAME);

        if std::fs::metadata(&temp_db_path).is_ok() {
            match std::fs::remove_dir_all(&db_path) {
                Ok(_) => {}
                Err(_) => {}
            }
            match std::fs::rename(&temp_db_path, &db_path) {
                Ok(_) => {}
                Err(err) => {
                    return Err(VacuumError(sled::Error::ReportableBug(format!(
                        "Failed renaming offsets db, Error: {:?}",
                        err
                    ))));
                }
            }
            return Ok(true);
        }

        Ok(false)
    }

    #[allow(dead_code)]
    fn vec_8_to_u16(&self, bytes: &[u8]) -> Vec<U16<BigEndian>> {
        // Ensure that the number of bytes is divisible by 2 (because each U16 takes 2 bytes)
        assert_eq!(bytes.len() % 2, 0);

        // Create a slice of U16 values from the byte slice
        let u16_slice = unsafe {
            // This is safe because we've already ensured that the byte slice is divisible by 2
            std::slice::from_raw_parts(bytes.as_ptr() as *const U16<BigEndian>, bytes.len() / 2)
        };
        // Print the U16 values
        // for u16_value in u16_slice {
        //     println!("{}", u16_value.get());
        //     foo.puu16_value.get()
        // }

        u16_slice.to_owned()
        // let mut result = U16::<BigEndian>::new(0);
        // result.extend_from_slice(u16_slice);
    }

    pub fn build_key(&self, key: &OffsetKey) -> String {
        // let namespace = ;
        // let partition = self.vec_8_to_u16(partition.as_bytes());
        // let key: Key = Key { namespace: namespace.to_string(), partition: partition.to_string() };
        // let key = Key { namespace: namespace.to_string(), partition: partition.to_string() };
        // key
        format!("{}-{}", key.namespace, key.partition)
    }

    pub fn build_latest_key(key: &OffsetKey) -> String {
        // let namespace = namespace.as_bytes();
        // let partition = partition.as_bytes();
        // unsafe self.any_as_u8_slice(format!("{}-{}-latest", namespace, partition).as_bytes().to_owned())
        // let partition = &format!("{}-latest", partition.to_owned());
        // let key: Key = Key { namespace: namespace.to_string(), partition: partition.to_string() };
        // key
        format!("{}-{}-latest", key.namespace, key.partition)
    }

    pub fn flush(&self) -> Option<usize> {
        match self.tree.flush() {
            Ok(val) => Some(val),
            Err(err) => {
                println!("Failed flushing offsets, Error: {:?}", err);
                None
            }
        }
    }
    pub fn set(&self, key: &OffsetKey, offset_type: OffsetTypes, offset: u64) -> Option<IVec> {
        match self.upsert(key, offset_type, offset) {
            Ok(val) => val,
            Err(err) => {
                println!("Failed setting offset, Error: {:?}", err);
                None
            }
        }
    }

    fn increment(&self, old: U64<LittleEndian>, new: U64<LittleEndian>) -> U64<LittleEndian> {
        if new.get() > old.get() {
            new
        } else {
            old
        }
    }

    pub fn insert(&self, key: &OffsetKey, offset_type: OffsetTypes, offset: u64) -> Option<IVec> {
        let key = self.build_key(key);
        let bytes: &[u8] = key.as_bytes();
        // let bytes: &[u8] = unsafe { self.any_as_u8_slice(&key) };

        let new_val = match offset_type {
            // @todo - deprecated, we never implement Filesize
            OffsetTypes::Filesize => sled::IVec::from(
                OffsetValue {
                    filesize: U64::new(offset),
                    line: U64::new(0),
                    closed: U64::new(0),
                }
                .as_bytes(),
            ),

            // @todo - compare and swap, we should only insert offsets that are greater than value in db
            OffsetTypes::Position => {
                let value_opt = self.tree.get(bytes).unwrap();
                // self.tree.f(bytes, |value_opt| {
                if let Some(existing) = value_opt {
                    let mut backing_bytes = sled::IVec::from(existing);

                    let layout: LayoutVerified<&mut [u8], OffsetValue> =
                        LayoutVerified::new_unaligned(&mut *backing_bytes)
                            .expect("bytes do not fit schema");

                    let old_value: &mut OffsetValue = layout.into_mut();

                    let new_value = self.increment(old_value.line, offset.into());

                    sled::IVec::from(
                        OffsetValue {
                            filesize: U64::new(0),
                            line: new_value,
                            closed: U64::new(0),
                        }
                        .as_bytes(),
                    )
                } else {
                    sled::IVec::from(
                        OffsetValue {
                            filesize: U64::new(0),
                            line: U64::new(offset),
                            closed: U64::new(0),
                        }
                        .as_bytes(),
                    )
                }
            }
            OffsetTypes::Closed => sled::IVec::from(
                OffsetValue {
                    filesize: U64::new(0),
                    line: U64::new(0),
                    closed: U64::new(offset),
                }
                .as_bytes(),
            ),
        };

        let old_val = self.tree.insert(bytes, &new_val).unwrap();

        old_val
    }

    pub fn get(&self, key: &OffsetKey) -> Option<IVec> {
        let key = self.build_key(key);
        // let bytes: &[u8] = unsafe { self.any_as_u8_slice(&key) };
        let bytes: &[u8] = key.as_bytes();
        self.tree.get(bytes).unwrap_or_else(|_err| {
            // println!("Failed getting offset, Error: {:?}", err);
            None
        })
    }

    pub fn get_line(&self, key: &OffsetKey) -> Option<U64<LittleEndian>> {
        let key = self.build_key(key);
        // let bytes: &[u8] = unsafe { self.any_as_u8_slice(&key) };
        let bytes: &[u8] = key.as_bytes();
        match self.tree.get(bytes) {
            Ok(val) => {
                let mut backing_bytes = sled::IVec::from(val.unwrap());

                // this verifies that our value is the correct length
                // and alignment (in this case we don't need it to be
                // aligned, because we use the `U64` type from zerocopy)
                let layout: LayoutVerified<&mut [u8], OffsetValue> =
                    LayoutVerified::new_unaligned(&mut *backing_bytes)
                        .expect("bytes do not fit schema");

                let value: &mut OffsetValue = layout.into_mut();
                Some(value.line)
            }
            Err(err) => {
                println!("Failed getting offset, Error: {:?}", err);
                None
            }
        }
    }

    pub fn get_latest(&self, key: &OffsetKey) -> Option<IVec> {
        let key = self.build_key(key);
        // let bytes: &[u8] = unsafe { self.any_as_u8_slice(&key) };
        let bytes: &[u8] = key.as_bytes();
        match self.tree.get(bytes) {
            Ok(val) => val,
            Err(err) => {
                println!("Failed getting latest offset, Error: {:?}", err);
                None
            }
        }
    }

    pub fn remove(&self, key: &OffsetKey) -> Result<Option<IVec>, sled::Error> {
        let key = self.build_key(key);
        let bytes: &[u8] = key.as_bytes();

        match self.tree.remove(bytes) {
            Ok(val) => Ok(val),
            // @todo - enumerate the possible sled::Error errors that can occur
            Err(err) => Err(sled::Error::ReportableBug(format!(
                "Failed removing offset for key {:?}, Error: {:?}",
                key, err
            ))),
        }
    }

    #[allow(dead_code)]
    unsafe fn any_as_u8_slice<T: Sized>(&self, p: &T) -> &[u8] {
        ::core::slice::from_raw_parts((p as *const T) as *const u8, ::core::mem::size_of::<T>())
    }

    #[allow(dead_code)]
    fn u64_to_ivec(number: u64) -> IVec {
        IVec::from(number.to_be_bytes().to_vec())
    }

    // fn ivec_to_u64(ivec: IVec) -> u64 {
    //     U64::from(ivec).into()
    // }
    pub fn validate(
        &self,
        key: &OffsetKey,
        offset_type: OffsetTypes,
        offset_value: u64,
    ) -> Option<bool> {
        // self.build_key(namespace, partition);

        let resp = match self.get(key) {
            Some(existing) => {
                // We need to make a copy that will be written back
                // into the database. This allows other threads that
                // may have witnessed the old version to keep working
                // without taking out any locks. IVec will be
                // stack-allocated until it reaches 22 bytes
                let mut backing_bytes = existing;

                // this verifies that our value is the correct length
                // and alignment (in this case we don't need it to be
                // aligned, because we use the `U64` type from zerocopy)
                let layout: LayoutVerified<&mut [u8], OffsetValue> =
                    LayoutVerified::new_unaligned(&mut *backing_bytes)
                        .expect("bytes do not fit schema");

                // this lets us work with the underlying bytes as
                // a mutable structured value.
                let value: &mut OffsetValue = layout.into_mut();

                let mut bool = false;

                match offset_type {
                    OffsetTypes::Filesize => {
                        if value.filesize.get() < offset_value {
                            // Some(false)
                            // println!("Setting filesize {}", filesize);
                            bool = true
                        }
                    }
                    OffsetTypes::Position => {
                        if value.line.get() < offset_value {
                            // Some(false)
                            // println!("Setting filesize {}", filesize);
                            bool = true
                        }
                    }
                    OffsetTypes::Closed => {
                        if value.closed.get() == offset_value {
                            // Some(false)
                            bool = true
                        }
                    }
                }

                Some(bool)
            }
            None => None,
        };

        resp
    }

    pub fn upsert(
        &self,
        key: &OffsetKey,
        offset_type: OffsetTypes,
        offset: u64,
    ) -> Result<Option<IVec>, sled::Error> {
        // let key = Key { namespace: namespace.to_string(), partition: partition.to_string() };
        // let bytes: &[u8] = unsafe { self.any_as_u8_slice(&key) };

        let key = self.build_key(key);
        let bytes: &[u8] = key.as_bytes();
        // let bytes: &[u8] = unsafe { self.any_as_u8_slice(&key) };

        // "UPSERT" functionality
        // let resp = self.tree.update_and_fetch(bytes, |value_opt| {
        self.tree.fetch_and_update(bytes, |value_opt| {
            if let Some(existing) = value_opt {
                // We need to make a copy that will be written back
                // into the database. This allows other threads that
                // may have witnessed the old version to keep working
                // without taking out any locks. IVec will be
                // stack-allocated until it reaches 22 bytes
                let mut backing_bytes = sled::IVec::from(existing);

                // this verifies that our value is the correct length
                // and alignment (in this case we don't need it to be
                // aligned, because we use the `U64` type from zerocopy)
                let layout: LayoutVerified<&mut [u8], OffsetValue> =
                    LayoutVerified::new_unaligned(&mut *backing_bytes)
                        .expect("bytes do not fit schema");

                // this lets us work with the underlying bytes as
                // a mutable structured value.
                let value: &mut OffsetValue = layout.into_mut();

                // println!("Updating offset");

                match offset_type {
                    OffsetTypes::Filesize => {
                        value.filesize.set(offset);
                    }
                    OffsetTypes::Position => {
                        value.line.set(offset);
                    }
                    OffsetTypes::Closed => {
                        value.closed.set(offset);
                    }
                }

                Some(backing_bytes)
                // Some(value)
                // Some(is_updated)
            } else {
                // println!("Creating offset");

                let new_val = sled::IVec::from(
                    OffsetValue {
                        filesize: U64::new(0),
                        line: U64::new(0),
                        closed: U64::new(0),
                    }
                    .as_bytes(),
                );

                self.tree.insert(bytes, &new_val).unwrap();

                Some(new_val)

                // Some(true)
            }
        })
    }
}

#[cfg(test)]
mod tests {

    use crate::helpers::offsets::{OffsetKey, OffsetTypes, OffsetValue, Offsets};

    use serial_test::serial;
    use zerocopy::{AsBytes, U64};

    #[test]
    #[serial]
    fn test_insert_position() {
        let db = match Offsets::init() {
            Ok(offsets) => offsets,
            Err(e) => {
                println!("Skipping: {}", e);
                return;
            }
        };

        db.tree.clear().unwrap();

        let key = &OffsetKey {
            namespace: "foo".to_string(),
            partition: "bar".to_string(),
        };

        assert_eq!(db.insert(key, OffsetTypes::Position, 1), None);
        assert_eq!(db.validate(key, OffsetTypes::Position, 1), Some(false));
        assert_eq!(db.validate(key, OffsetTypes::Position, 2), Some(true));

        let return_val = sled::IVec::from(
            OffsetValue {
                filesize: U64::new(0),
                line: U64::new(1),
                closed: U64::new(0),
            }
            .as_bytes(),
        );
        assert_eq!(db.insert(key, OffsetTypes::Position, 2), Some(return_val));
        assert_eq!(db.validate(key, OffsetTypes::Position, 1), Some(false));
        assert_eq!(db.validate(key, OffsetTypes::Position, 2), Some(false));
        assert_eq!(db.validate(key, OffsetTypes::Position, 3), Some(true));

        let return_val = sled::IVec::from(
            OffsetValue {
                filesize: U64::new(0),
                line: U64::new(2),
                closed: U64::new(0),
            }
            .as_bytes(),
        );
        assert_eq!(db.insert(key, OffsetTypes::Position, 3), Some(return_val));
        assert_eq!(db.validate(key, OffsetTypes::Position, 1), Some(false));
        assert_eq!(db.validate(key, OffsetTypes::Position, 2), Some(false));
        assert_eq!(db.validate(key, OffsetTypes::Position, 3), Some(false));
        assert_eq!(db.validate(key, OffsetTypes::Position, 4), Some(true));

        let return_val = sled::IVec::from(
            OffsetValue {
                filesize: U64::new(0),
                line: U64::new(3),
                closed: U64::new(0),
            }
            .as_bytes(),
        );
        assert_eq!(db.insert(key, OffsetTypes::Position, 3), Some(return_val));

        let return_val = sled::IVec::from(
            OffsetValue {
                filesize: U64::new(0),
                line: U64::new(3),
                closed: U64::new(0),
            }
            .as_bytes(),
        );
        assert_eq!(db.insert(key, OffsetTypes::Position, 2), Some(return_val));
    }

    #[test]
    #[serial]
    fn test_validate() {
        let db = match Offsets::init() {
            Ok(offsets) => offsets,
            Err(e) => {
                println!("Skipping: {}", e);
                return;
            }
        };

        db.tree.clear().unwrap();

        // assert_eq!(db.validate(key, 1, 1), Some(true));
        // assert_eq!(db.validate(key, 1, 1), Some(false)); // @todo this is atleast once
        // assert_eq!(db.validate(key, 2, 1), Some(true));
        // assert_eq!(db.validate(key, 1, 2), Some(false));
        // assert_eq!(db.validate(key, 2, 1), Some(false));
        // assert_eq!(db.validate(key, 2, 2), Some(true)); // @todo this is atleast once
        // assert_eq!(db.validate(key, 2, 3), Some(true));
        // assert_eq!(db.validate(key, 2, 2), Some(false));
        // assert_eq!(db.validate(key, 1, 4), Some(false));

        let key = &OffsetKey {
            namespace: "foo".to_string(),
            partition: "bar".to_string(),
        };

        assert_eq!(db.validate(key, OffsetTypes::Filesize, 1), None);
        db.set(key, OffsetTypes::Filesize, 1).unwrap();
        assert_eq!(db.validate(key, OffsetTypes::Filesize, 1), Some(false));
        assert_eq!(db.validate(key, OffsetTypes::Filesize, 2), Some(true));
        db.set(key, OffsetTypes::Filesize, 2).unwrap();
        assert_eq!(db.validate(key, OffsetTypes::Filesize, 1), Some(false));
        assert_eq!(db.validate(key, OffsetTypes::Filesize, 2), Some(false));
        assert_eq!(db.validate(key, OffsetTypes::Filesize, 3), Some(true));
        assert_eq!(db.validate(key, OffsetTypes::Filesize, 4), Some(true));
        assert_eq!(db.validate(key, OffsetTypes::Filesize, 2), Some(false));

        let key = &OffsetKey {
            namespace: "foo".to_string(),
            partition: "bar2".to_string(),
        };

        assert_eq!(db.validate(key, OffsetTypes::Closed, 0), None);
        assert_eq!(db.validate(key, OffsetTypes::Closed, 1), None);
        db.set(key, OffsetTypes::Closed, 1).unwrap();
        assert_eq!(db.validate(key, OffsetTypes::Closed, 1), Some(true));
        assert_eq!(db.validate(key, OffsetTypes::Closed, 0), Some(false));

        // db.remove(key).unwrap();

        db.tree.flush().unwrap();
        // db.db.flush().unwrap();
        // drop(db.db);
        // drop(db.tree);
        // drop(db);
    }

    // #[test]
    // #[serial]
    // fn test_validate_performance() {
    //
    //     let ingestMsgCount = Arc::new(Mutex::new(0));
    //     let ingestMsgCountClone = ingestMsgCount.clone();
    //     let skippedMsgCount = Arc::new(Mutex::new(0));
    //     let skippedMsgCountClone = skippedMsgCount.clone();
    //     let ingestMsgTotal = Arc::new(Mutex::new(0));
    //     let now = Arc::new(Mutex::new(Instant::now()));
    //
    //     let mut planner = periodic::Planner::new();
    //
    //     planner.add(
    //         move || {
    //             let mut metrics: Metrics = Metrics::new();
    //
    //             let mut counter_lock = ingestMsgCount.lock().unwrap();
    //             let mut skipped_lock = skippedMsgCount.lock().unwrap();
    //             let mut total_lock = ingestMsgTotal.lock().unwrap();
    //             let now_lock = now.lock().unwrap();
    //
    //             *total_lock += *counter_lock;
    //
    //             metrics.msgs_total = *total_lock;
    //             metrics.msgs_current = *counter_lock;
    //             metrics.deadletters_current = *skipped_lock;
    //             metrics.run_time_seconds = now_lock.elapsed().as_secs().clone() as i64;
    //
    //             println!("Runtime: {} seconds", now_lock.elapsed().as_secs());
    //             println!("Skipped Messages: {}", *skipped_lock);
    //             println!("Ingested Messages: {}", *counter_lock);
    //             println!("Total Messages: {}", *total_lock);
    //
    //             *counter_lock = 0;
    //
    //
    //         },
    //         periodic::Every::new(Duration::from_secs(60)),
    //     );
    //     planner.start();
    //
    //
    //     let db = Offsets::init().unwrap();
    //
    //     // for i in 1..=100000000 {
    //     for i in 1..=10000000 {
    //     // for i in 1..=100 {
    //
    //         // let mut rng = rand::thread_rng();
    //         // let y: f64 = rng.gen(); // generates a float between 0 and 1
    //         // let foo = 10.0 * y;
    //
    //         let y = i + 1;
    //
    //         match db.validate("abc", &i.to_string(), OffsetTypes::Filesize, y) {
    //             Some(true) => {
    //                 // println!("True: {}", i);
    //                 let mut counter_lock = ingestMsgCountClone.lock().unwrap();
    //                 *counter_lock += 1;
    //                 db.set("abc", &i.to_string(), OffsetTypes::Filesize, y);
    //             },
    //             Some(false) => {
    //                 // println!("False: {}", i);
    //                 let mut counter_lock = skippedMsgCountClone.lock().unwrap();
    //                 *counter_lock += 1;
    //             },
    //             None => {
    //                 db.insert("abc", &i.to_string(), OffsetTypes::Filesize, y);
    //                 // println!("None: {}", i);
    //             }
    //         }
    //
    //     }
    //
    //     let mut counter_lock = ingestMsgCountClone.lock().unwrap();
    //     let mut skipped_lock = skippedMsgCountClone.lock().unwrap();
    //
    //     println!("Skipped Messages: {}", *skipped_lock);
    //     println!("Ingested Messages: {}", *counter_lock);
    //
    //     db.tree.flush().unwrap();
    //     db.db.flush().unwrap();
    //     drop(db.tree);
    //     drop(db.db);
    //     // drop(db);
    // }
}
