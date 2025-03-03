use proptest::prelude::*;
use std::io::Cursor;

use super::super::*;

use crate::serialization::{ZcashDeserialize, ZcashDeserializeInto, ZcashSerialize};

proptest! {
    #[test]
    fn transaction_roundtrip(tx in any::<Transaction>()) {
        zebra_test::init();

        let data = tx.zcash_serialize_to_vec().expect("tx should serialize");
        let tx2 = data.zcash_deserialize_into().expect("randomized tx should deserialize");

        prop_assert_eq![tx, tx2];
    }

    #[test]
    fn transaction_hash_display_fromstr_roundtrip(hash in any::<Hash>()) {
        zebra_test::init();

        let display = format!("{}", hash);
        let parsed = display.parse::<Hash>().expect("hash should parse");
        prop_assert_eq!(hash, parsed);
    }

    #[test]
    fn locktime_roundtrip(locktime in any::<LockTime>()) {
        zebra_test::init();

        let mut bytes = Cursor::new(Vec::new());
        locktime.zcash_serialize(&mut bytes)?;

        bytes.set_position(0);
        let other_locktime = LockTime::zcash_deserialize(&mut bytes)?;

        prop_assert_eq![locktime, other_locktime];
    }
}
