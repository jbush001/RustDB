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
    And, Or,
    In, NotIn
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
            Operation::Or => write!(f, "or")?,
            Operation::In => write!(f, "in")?,
            Operation::NotIn => write!(f, "not in")?
        }

        Ok(())
    }
}

#[derive(Debug)]
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

fn get_bool_result(value: &Value) -> Result<bool, String> {
    value.as_bool().ok_or("Invalid type for boolean expr".to_string())
}

impl ExpressionNode {
    fn eval(&self, document: &Value) -> Result<Value, String> {
        match self {
            ExpressionNode::Constant(val) => Ok(val.clone()),
            ExpressionNode::Path(path) => lookup_field(path, document),
            ExpressionNode::BinaryOp((operation, left, right)) => {
                let left_val = left.eval(document)?;
                let right_val = right.eval(document)?;

                if matches!(operation, Operation::In) || matches!(operation, Operation::NotIn) {
                    if let Value::Array(items) = right_val {
                        for item in items {
                            if item == left_val {
                                return Ok(Value::Bool(matches!(operation, Operation::In)));
                            }
                        }

                        Ok(Value::Bool(matches!(operation, Operation::NotIn)))
                    } else {
                        Err("RHS of in expression must be an array".to_string())
                    }
                } else {
                    // Comparisons must obey the same ordering as the btree encoding, so we
                    // encoded them the same with a dummmy docid. This means we will not
                    // coerce values for comparison.
                    if left_val.is_null() || right_val.is_null() {
                        // Always return true for comparisons where value is null.
                        return Ok(Value::Bool(true));
                    }

                    match operation {
                        Operation::And => {
                            return Ok(Value::Bool(get_bool_result(&left_val)?
                                && get_bool_result(&right_val)?))
                        },
                        Operation::Or => {
                            return Ok(Value::Bool(get_bool_result(&left_val)?
                                || get_bool_result(&right_val)?))
                        },
                        _ => {} // Falls through
                    }

                    let encoded_left = encode_key(&left_val, DocId(0))?;
                    let encoded_right = encode_key(&right_val, DocId(0))?;
                    match operation {
                        Operation::Gt => Ok(Value::Bool(encoded_left > encoded_right)),
                        Operation::Gte => Ok(Value::Bool(encoded_left >= encoded_right)),
                        Operation::Lt => Ok(Value::Bool(encoded_left < encoded_right)),
                        Operation::Lte => Ok(Value::Bool(encoded_left <= encoded_right)),
                        Operation::Eq => Ok(Value::Bool(encoded_left == encoded_right)),
                        Operation::Neq => Ok(Value::Bool(encoded_left != encoded_right)),
                        _ => { unreachable!(); }
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
            let result = self.expression.eval(&doc).unwrap_or(Value::Bool(false));
            if get_bool_result(&result).unwrap_or(false) {
                return Some((docid, doc))
            }
        }
    }
}

fn doc2expr(value: &Value) -> Result<ExpressionNode, String> {
    if let Value::Array(vec) = value {
        let operation = vec[0].as_str().ok_or("Invalid operation type".to_string())?;

        // Check for unary ops
        match operation {
            "get" => {
                return Ok(ExpressionNode::Path(FieldPath::new(vec[1].as_str()
                    .ok_or(format!("Invalid field name {:?}", vec[1]))?)?));
            },
            "const" => {
                return Ok(ExpressionNode::Constant(vec[1].clone()));
            },
            _ => {} // falls through
        };

        // Binary operation
        let opcode = match operation {
            "and" => Operation::And,
            "or" => Operation::Or,
            "gt" => Operation::Gt,
            "ge" => Operation::Gte,
            "lt" => Operation::Lt,
            "le" => Operation::Lte,
            "eq" => Operation::Eq,
            "ne" => Operation::Neq,
            _ => { return Err(format!("Bad operation {}", operation)); }
        };

        Ok(ExpressionNode::BinaryOp((
            opcode,
            Box::new(doc2expr(&value[1])?),
            Box::new(doc2expr(&value[2])?),
        )))
    } else {
        return Err(format!("Invalid expression {}", value));
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
    fn  test_invalid_bool_and() {
        let expr = ExpressionNode::BinaryOp((
            Operation::And,
            Box::new(ExpressionNode::Constant(json!(12))),
            Box::new(ExpressionNode::Constant(Value::Bool(true)))
        ));

        assert_eq!(expr.eval(&json!({})),
            Err("Invalid type for boolean expr".to_string()));
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

        assert_eq!(format!("{}", ExpressionNode::BinaryOp((Operation::In,
            Box::new(ExpressionNode::Path(FieldPath::new("frob").expect("error creating field path"))),
            Box::new(ExpressionNode::Constant(json!(["a", "b", "c", "d", "e"])))))),
            r#"(frob in ["a","b","c","d","e"])"#);
        assert_eq!(format!("{}", ExpressionNode::BinaryOp((Operation::NotIn,
            Box::new(ExpressionNode::Path(FieldPath::new("frob").expect("error creating field path"))),
            Box::new(ExpressionNode::Constant(json!([1,2,3,4,5])))))),
            "(frob not in [1,2,3,4,5])");
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
                    Value::Bool(left || right),
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

            // Null (always returns true)
            (Operation::Lt, Value::Null, json!(3), true),
            (Operation::Lt, json!(3), Value::Null, true),
            (Operation::Lte, Value::Null, json!(3), true),
            (Operation::Lte, json!(3), Value::Null, true),
            (Operation::Gt, Value::Null, json!(3), true),
            (Operation::Gte, json!(3), Value::Null, true),
            (Operation::Eq, Value::Null, json!(3), true),
            (Operation::Eq, json!(3), Value::Null, true),
            (Operation::Neq, Value::Null, json!(3), true),
            (Operation::Neq, json!(3), Value::Null, true)
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

    #[test]
    fn test_in_not_in() {
        let expr = ExpressionNode::BinaryOp((
            Operation::In,
            Box::new(ExpressionNode::Constant(json!(3))),
            Box::new(ExpressionNode::Constant(json!([1, 3, 5, 7, 9])))
        ));

        assert_eq!(expr.eval(&json!({})).unwrap(), Value::Bool(true));

        let expr = ExpressionNode::BinaryOp((
            Operation::In,
            Box::new(ExpressionNode::Constant(json!(4))),
            Box::new(ExpressionNode::Constant(json!([1, 3, 5, 7, 9])))
        ));

        assert_eq!(expr.eval(&json!({})).unwrap(), Value::Bool(false));

        let expr = ExpressionNode::BinaryOp((
            Operation::NotIn,
            Box::new(ExpressionNode::Constant(json!(3))),
            Box::new(ExpressionNode::Constant(json!([1, 3, 5, 7, 9])))
        ));

        assert_eq!(expr.eval(&json!({})).unwrap(), Value::Bool(false));

        let expr = ExpressionNode::BinaryOp((
            Operation::NotIn,
            Box::new(ExpressionNode::Constant(json!(4))),
            Box::new(ExpressionNode::Constant(json!([1, 3, 5, 7, 9])))
        ));

        assert_eq!(expr.eval(&json!({})).unwrap(), Value::Bool(true));
    }

    #[test]
    fn test_in_not_in_bad_param() {
        let expr = ExpressionNode::BinaryOp((
            Operation::In,
            Box::new(ExpressionNode::Constant(json!(3))),
            Box::new(ExpressionNode::Constant(json!(12)))
        ));

        assert_eq!(expr.eval(&json!({})).unwrap_err(), "RHS of in expression must be an array".to_string());

        let expr = ExpressionNode::BinaryOp((
            Operation::NotIn,
            Box::new(ExpressionNode::Constant(json!(3))),
            Box::new(ExpressionNode::Constant(json!({"foo": 3})))
        ));

        assert_eq!(expr.eval(&json!({})).unwrap_err(), "RHS of in expression must be an array".to_string());
    }

    #[test]
    fn test_doc2expr() {
        let expr = doc2expr(
            &json!(["and", ["gt", ["get", "foo.bar"], ["const", 10]], ["le", ["get", "baz"], ["const", "boo"]]]));
        assert_eq!(&expr.unwrap().to_string(), "((foo.bar > 10) and (baz <= \"boo\"))");
    }

    #[test]
    fn test_doc2expr_invalid_field() {
         assert_eq!(doc2expr(&json!(["get", "$---/>"])).unwrap_err(),
            "Invalid path element: $---/>");
    }

    #[test]
    fn test_invalid_op_type() {
         assert_eq!(doc2expr(&json!([12])).unwrap_err(),
            "Invalid operation type");
    }

    #[test]
    fn test_invalid_operation() {
         assert_eq!(doc2expr(&json!(["frobulate", 12])).unwrap_err(),
            "Bad operation frobulate");
    }

    #[test]
    fn test_invalid_type() {
         assert_eq!(doc2expr(&json!(12)).unwrap_err(),
            "Invalid expression 12");
    }
}
