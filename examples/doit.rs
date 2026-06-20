//
//   Copyright 2026 Jeff Bush
//
//   Licensed under the Apache License, Version 2.0 (the "License");
//   you may not use this file except in compliance with the License.
//   You may obtain a copy of the License at
//
//       http://www.apache.org/licenses/LICENSE-2.0
//
//   Unless required by applicable law or agreed to in writing, software
//   distributed under the License is distributed on an "AS IS" BASIS,
//   WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
//   See the License for the specific language governing permissions and
//   limitations under the License.
//

use rustdb::*;
use serde_json::*;

fn main() {
    let mut db = Database::open("/tmp/foo.db").unwrap();

    if !db.get_collection_list().contains(&"people".to_string()) {
        let _transaction = db.begin_transaction();
        let _ = db.create_collection("people");
        db.create_index("people", "name").unwrap();
    }

    {
        let _transaction = db.begin_transaction();
        db.insert_document("people", &json!({"name": "Alice", "age": 30})).unwrap();
        db.insert_document("people", &json!({"name": "Bob", "age": 25})).unwrap();
        db.insert_document("people", &json!({"name": "Charlie", "age": 35})).unwrap();
    }

    let iter = db.seq_iter("people").expect("error in query");
    for (docid, value) in iter {
        println!("{} {}", docid.0, value);
    }
}
