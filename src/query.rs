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

use crate::collection::*;
use serde_json::Value;

#[derive(Debug, Clone)]
enum Operation {
    Gt, Gte, Lt, Lte, Eq, Neq,
    And, Or
}

impl std::fmt::Display for Operation {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> Result<(), std::fmt::Error> {
        match self {
            Operation::Gt => write!(f, ">")?,
            Operation::Gte => write!(f, ">=")?,
            Operation::Lt => write!(f, "<")?,
            Operation::Lte => write!(f, "<=")?,
            Operation::Eq => write!(f, "=")?,
            Operation::Neq => write!(f, "<>")?,
            Operation::And => write!(f, "and")?,
            Operation::Or => write!(f, "or")?
        }

        Ok(())
    }
}

enum ExpressionNode {
    BinaryOp((Operation, Box<ExpressionNode>, Box<ExpressionNode>)),
    Path(FieldPath),
    Constant(Value)
}

impl std::fmt::Display for ExpressionNode {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> Result<(), std::fmt::Error> {
        match self {
            ExpressionNode::BinaryOp((operation, left, right)) => {
                write!(f, "({} {} {})", left, operation, right)?
            },
            ExpressionNode::Path(path) => write!(f, "{}", path)?,
            ExpressionNode::Constant(val) => write!(f, "{}", val)?
        }

        Ok(())
    }
}

fn cast_to_bool(value: &Value) -> bool {
    match value {
        Value::Bool(b) => *b,
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                i != 0
            } else if let Some(f) = n.as_f64() {
                f != 0.0
            } else {
                false // Is this reachable?
            }
        },
        Value::String(s) => !s.is_empty(),
        Value::Array(arr) => !arr.is_empty(),
        Value::Object(obj) => !obj.is_empty(),
        Value::Null => false
    }
}

impl ExpressionNode {
    fn eval(&self, document: &Value) -> Result<Value, String> {
        match self {
            ExpressionNode::Constant(val) => Ok(val.clone()),
            ExpressionNode::Path(path) => lookup_field(path, document),
            ExpressionNode::BinaryOp((operation, left, right)) => {
                // Comparisons must obey the same ordering as the btree encoding, so we
                // encoded them the same with a dummmy docid. This means we will not
                // coerce values for comparison.
                let encoded_left = encode_key(&left.eval(document)?, DocId(0))?;
                let encoded_right = encode_key(&right.eval(document)?, DocId(0))?;
                match operation {
                    Operation::Gt => Ok(Value::Bool(encoded_left > encoded_right)),
                    Operation::Gte => Ok(Value::Bool(encoded_left >= encoded_right)),
                    Operation::Lt => Ok(Value::Bool(encoded_left < encoded_right)),
                    Operation::Lte => Ok(Value::Bool(encoded_left <= encoded_right)),
                    Operation::Eq => Ok(Value::Bool(encoded_left == encoded_right)),
                    Operation::Neq => Ok(Value::Bool(encoded_left != encoded_right)),
                    Operation::And => {
                        Ok(Value::Bool(cast_to_bool(&left.eval(document)?)
                            && cast_to_bool(&right.eval(document)?)))
                    },
                    Operation::Or => {
                        Ok(Value::Bool(cast_to_bool(&left.eval(document)?)
                            || cast_to_bool(&right.eval(document)?)))
                    }
                }
            }
        }
    }
}

struct ExpressionFilter {
    source: Box<dyn Iterator<Item = (DocId, Value)>>,
    expression: Box<ExpressionNode>
}

impl Iterator for ExpressionFilter {
    type Item = (DocId, Value);

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let (docid, doc) = self.source.next()?;
            let result = self.expression.eval(&doc);
            if cast_to_bool(&result.unwrap_or(Value::Bool(false))) {
                return Some((docid, doc))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};
    use super::*;

    struct MockSource {
        documents: Vec<(DocId, Value)>,
        index: usize
    }

    impl Iterator for MockSource {
        type Item = (DocId, Value);

        fn next(&mut self) -> Option<Self::Item> {
            if self.index >= self.documents.len() {
                None
            } else {
                let result = self.documents[self.index].clone();
                self.index += 1;
                Some(result)
            }
        }
    }

    #[test]
    fn test_expression_filter() {
        let documents = vec![
            json!({"foo": 1, "bar": 99}),
            json!({"foo": 2, "bar": 100}),
            json!({"foo": 3, "bar": 101}),
            json!({"foo": 4, "bar": 102}),
            json!({"foo": 5, "bar": 103}),
            json!({"foo": 6, "bar": 104}),
            json!({"foo": 7, "bar": 105}),
            json!({"foo": 8, "bar": 106}),
            json!({"foo": 9, "bar": 107})
        ];

        let source = MockSource {
            documents: (1..).into_iter().zip(documents).map(|(docid, doc)| (DocId(docid), doc)).collect(),
            index: 0
        };
        let filter = ExpressionFilter {
            source: Box::new(source),
            expression: Box::new(ExpressionNode::BinaryOp((
                Operation::Or,
                Box::new(ExpressionNode::BinaryOp((
                    Operation::Eq,
                    Box::new(ExpressionNode::Path(FieldPath::new("foo").expect("error creating field path"))),
                    Box::new(ExpressionNode::Constant(json!(9)))
                ))),
                Box::new(ExpressionNode::BinaryOp((
                    Operation::And,
                    Box::new(ExpressionNode::BinaryOp((
                        Operation::Gt,
                        Box::new(ExpressionNode::Path(FieldPath::new("foo").expect("error creating field path"))),
                        Box::new(ExpressionNode::Constant(json!(2)))
                    ))),
                    Box::new(ExpressionNode::BinaryOp((
                        Operation::Lt,
                        Box::new(ExpressionNode::Path(FieldPath::new("bar").expect("error creating field path"))),
                        Box::new(ExpressionNode::Constant(json!(105)))
                    )))
                )))
            )))
        };

        let results: Vec<(DocId, Value)> = filter.collect();
        assert_eq!(results.len(), 5);
        assert_eq!(results[0].1, json!({"foo": 3, "bar": 101}));
        assert_eq!(results[1].1, json!({"foo": 4, "bar": 102}));
        assert_eq!(results[2].1, json!({"foo": 5, "bar": 103}));
        assert_eq!(results[3].1, json!({"foo": 6, "bar": 104}));
        assert_eq!(results[4].1, json!({"foo": 9, "bar": 107}));
    }

    #[test]
    fn test_cast_to_bool() {
        assert_eq!(cast_to_bool(&json!(true)), true);
        assert_eq!(cast_to_bool(&json!(false)), false);
        assert_eq!(cast_to_bool(&json!(0)), false);
        assert_eq!(cast_to_bool(&json!(1)), true);
        assert_eq!(cast_to_bool(&json!(0.0)), false);
        assert_eq!(cast_to_bool(&json!(0.1)), true);
        assert_eq!(cast_to_bool(&json!("")), false);
        assert_eq!(cast_to_bool(&json!("hello")), true);
        assert_eq!(cast_to_bool(&json!([])), false);
        assert_eq!(cast_to_bool(&json!([1])), true);
        assert_eq!(cast_to_bool(&json!({})), false);
        assert_eq!(cast_to_bool(&json!({"key": "value"})), true);
        assert_eq!(cast_to_bool(&Value::Null), false);
    }

    #[test]
    fn test_expression_display() {
        assert_eq!(
            format!("{}", ExpressionNode::BinaryOp((
                Operation::And,
                Box::new(ExpressionNode::BinaryOp((
                    Operation::Gt,
                    Box::new(ExpressionNode::Path(FieldPath::new("foo").expect("error creating field path"))),
                    Box::new(ExpressionNode::Constant(json!(2)))
                ))),
                Box::new(ExpressionNode::BinaryOp((
                    Operation::Lt,
                    Box::new(ExpressionNode::Path(FieldPath::new("bar").expect("error creating field path"))),
                    Box::new(ExpressionNode::Constant(json!(105)))
                )))
            ))),
            "((foo > 2) and (bar < 105))"
        );

        assert_eq!(
            format!("{}", ExpressionNode::BinaryOp((
                Operation::Or,
                Box::new(ExpressionNode::BinaryOp((
                    Operation::Gte,
                    Box::new(ExpressionNode::Path(FieldPath::new("foo").expect("error creating field path"))),
                    Box::new(ExpressionNode::Constant(json!(2)))
                ))),
                Box::new(ExpressionNode::BinaryOp((
                    Operation::Lte,
                    Box::new(ExpressionNode::Path(FieldPath::new("bar").expect("error creating field path"))),
                    Box::new(ExpressionNode::Constant(json!(105)))
                )))
            ))),
            "((foo >= 2) or (bar <= 105))"
        );

        assert_eq!(format!("{}", ExpressionNode::BinaryOp((Operation::Neq,
            Box::new(ExpressionNode::Path(FieldPath::new("baz").expect("error creating field path"))),
            Box::new(ExpressionNode::Constant(json!(9)))))),
            "(baz <> 9)");
        assert_eq!(format!("{}", ExpressionNode::BinaryOp((Operation::Eq,
            Box::new(ExpressionNode::Path(FieldPath::new("frob").expect("error creating field path"))),
            Box::new(ExpressionNode::Constant(json!(9)))))),
            "(frob = 9)");
    }

    #[test]
    fn test_and_eval() {
        for left in [true, false] {
            for right in [true, false] {
                let expr = ExpressionNode::BinaryOp((
                    Operation::And,
                    Box::new(ExpressionNode::Constant(Value::Bool(left))),
                    Box::new(ExpressionNode::Constant(Value::Bool(right)))
                ));

                assert_eq!(
                    expr.eval(&json!({})).unwrap(),
                    left && right,
                    "Failed for: {:?} AND {:?}", left, right
                );
            }
        }
    }

    #[test]
    fn test_or_eval() {
        for left in [true, false] {
            for right in [true, false] {
                let expr = ExpressionNode::BinaryOp((
                    Operation::Or,
                    Box::new(ExpressionNode::Constant(Value::Bool(left))),
                    Box::new(ExpressionNode::Constant(Value::Bool(right)))
                ));

                assert_eq!(
                    expr.eval(&json!({})).unwrap(),
                    left || right,
                    "Failed for: {:?} AND {:?}", left, right
                );
            }
        }
    }

    // Verifies operand types are handled cleanly. These are all converted to
    // binary codings that match the key types.
    #[test]
    fn test_comparison() {
        let ops = vec![
            // Integer
            (Operation::Lt, json!(2), json!(3), true),
            (Operation::Lt, json!(3), json!(3), false),
            (Operation::Lt, json!(4), json!(3), false),
            (Operation::Lte, json!(2), json!(3), true),
            (Operation::Lte, json!(3), json!(3), true),
            (Operation::Lte, json!(4), json!(3), false),
            (Operation::Gt, json!(2), json!(3), false),
            (Operation::Gt, json!(3), json!(3), false),
            (Operation::Gt, json!(4), json!(3), true),
            (Operation::Gte, json!(2), json!(3), false),
            (Operation::Gte, json!(3), json!(3), true),
            (Operation::Gte, json!(4), json!(3), true),
            (Operation::Eq, json!(3), json!(3), true),
            (Operation::Eq, json!(2), json!(3), false),
            (Operation::Neq, json!(3), json!(3), false),
            (Operation::Neq, json!(2), json!(3), true),

            // Floating point
            (Operation::Lt, json!(2.1), json!(3.1), true),
            (Operation::Lt, json!(3.1), json!(3.1), false),
            (Operation::Lt, json!(4.1), json!(3.1), false),
            (Operation::Lte, json!(2.1), json!(3.1), true),
            (Operation::Lte, json!(3.1), json!(3.1), true),
            (Operation::Lte, json!(4.1), json!(3.1), false),
            (Operation::Gt, json!(2.1), json!(3.1), false),
            (Operation::Gt, json!(3.1), json!(3.1), false),
            (Operation::Gt, json!(4.1), json!(3.1), true),
            (Operation::Gte, json!(2.1), json!(3.1), false),
            (Operation::Gte, json!(3.1), json!(3.1), true),
            (Operation::Gte, json!(4.1), json!(3.1), true),
            (Operation::Eq, json!(3.1), json!(3.1), true),
            (Operation::Eq, json!(2.1), json!(3.1), false),
            (Operation::Neq, json!(3.1), json!(3.1), false),
            (Operation::Neq, json!(2.1), json!(3.1), true),

            // String
            (Operation::Lt, json!("abc"), json!("abb"), false),
            (Operation::Lt, json!("abc"), json!("abc"), false),
            (Operation::Lt, json!("abc"), json!("abd"), true),
            (Operation::Lte, json!("abc"), json!("abb"), false),
            (Operation::Lte, json!("abc"), json!("abc"), true),
            (Operation::Lte, json!("abc"), json!("abd"), true),
            (Operation::Gt, json!("abc"), json!("abb"), true),
            (Operation::Gt, json!("abc"), json!("abc"), false),
            (Operation::Gt, json!("abc"), json!("abd"), false),
            (Operation::Gte, json!("abc"), json!("abb"), true),
            (Operation::Gte, json!("abc"), json!("abc"), true),
            (Operation::Gte, json!("abc"), json!("abd"), false),
            (Operation::Eq, json!("abc"), json!("abc"), true),
            (Operation::Eq, json!("abc"), json!("abd"), false),
            (Operation::Neq, json!("abc"), json!("abc"), false),
            (Operation::Neq, json!("abc"), json!("abd"), true),

            // Mixed types
            (Operation::Gt, json!(100), json!(false), true),
            (Operation::Gt, json!(3.14), json!(100), true),
            (Operation::Gt, json!("abcd"), json!(3.14), true),
        ];

        for (operation, operand1, operand2, expected) in ops {
            let expr = ExpressionNode::BinaryOp((
                operation.clone(),
                Box::new(ExpressionNode::Constant(operand1.clone())),
                Box::new(ExpressionNode::Constant(operand2.clone()))
            ));

            assert_eq!(expr.eval(&json!({})).unwrap(), Value::Bool(expected),
                "compare failed {:?} {:?} {:?}", operation.clone(), operand1.clone(), operand2.clone());
        }

    }
}
