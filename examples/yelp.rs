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
use std::env;
use std::fs::File;
use std::io::{BufRead, BufReader};

fn main() {
    let args: Vec<String> = env::args().collect();
    let mut db = Database::open("/tmp/yelp.db").unwrap();

    if !db.get_collection_list().contains(&"businesses".to_string()) {
        fill_database(&args[1], &mut db);
    }

    // Show all businesses in california with more than 4.0 stars
    let iter = db.index_iter("businesses", "state", Some(Value::String("CA".to_string())),
        Some(Value::String("CA".to_string())), false).expect("error in query");
    let filter = ExpressionFilter::new(
        Box::new(iter),
        Box::new(ExpressionNode::BinaryOp((
            Operation::And,
            Box::new(ExpressionNode::BinaryOp((
                Operation::Gte,
                Box::new(ExpressionNode::Path(FieldPath::new("stars").unwrap())),
                Box::new(ExpressionNode::Constant(json!(4.0)))
            ))),
            Box::new(ExpressionNode::BinaryOp((
                Operation::Eq,
                Box::new(ExpressionNode::Path(FieldPath::new("city").unwrap())),
                Box::new(ExpressionNode::Constant(json!("Santa Barbara")))
            )))
        )))
    );

    for (docid, value) in filter {
        println!("{} {}", docid.0, value);
    }
}

fn fill_database(file_name: &str, db: &mut Database) {
    {
        let _transaction = db.begin_transaction();
        db.create_collection("businesses").unwrap();
        db.create_index("businesses", "business_id").unwrap();
        db.create_index("businesses", "city").unwrap();
        db.create_index("businesses", "state").unwrap();
        db.create_index("businesses", "stars").unwrap();
        db.create_index("businesses", "postal_code").unwrap();
    }

    let file = File::open(file_name).expect("error opening file");
    let reader = BufReader::new(file);
    let mut count = 0;
    const BATCH_LENGTH: usize = 30;
    let mut batch = Vec::with_capacity(BATCH_LENGTH);
    for line in reader.lines() {
        let value: serde_json::Value = serde_json::from_str(&line.unwrap()).unwrap();
        batch.push(value);

        if batch.len() == BATCH_LENGTH {
            count += batch.len();
            println!("inserting record {}", count);

            let _transaction = db.begin_transaction();
            for doc in &batch {
                db.insert_document("businesses", doc).unwrap();
            }

            batch.clear();
        }
    }
}
