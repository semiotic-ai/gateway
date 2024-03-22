use anyhow::anyhow;
use cost_model::Context;
use gateway_framework::errors::Error;
use graphql::graphql_parser::query::OperationDefinition;

pub fn validate_sql_query(ctx: &Context<String>) -> Result<(), Error> {
    for operation in &ctx.operations {
        match operation {
            OperationDefinition::Query(query) => {
                let items_not_sql = query
                    .selection_set
                    .items
                    .iter()
                    .filter_map(|selection| {
                        if let graphql::graphql_parser::query::Selection::Field(field) = selection {
                            if field.name != "sql" {
                                return Some(field.name.as_str());
                            }
                        }
                        None
                    })
                    .collect::<Vec<_>>();
                if items_not_sql.len() > 0 {
                    return Err(Error::BadQuery(anyhow!(
                        "Fields [{}] are not SQL",
                        items_not_sql.join(", ")
                    )));
                }
            }
            _ => continue,
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_single_selection_set() {
        let query = r#"
            query {
                sql(input: { query: "SELECT * FROM users" }) {
                    id
                    name
                }
            }
        "#;
        let variables = r#"{}"#;
        let ctx = Context::new(query, variables).unwrap();
        assert!(validate_sql_query(&ctx).is_ok());
    }

    #[test]
    fn test_multi_selection_set() {
        let query = r#"
            query {
                sql(input: { query: "SELECT * FROM users" }) {
                    rows
                    columns
                }
                sql(input: { query: "SELECT * FROM tokens" }) {
                    id
                    name
                }
            }
        "#;
        let variables = r#"{}"#;
        let ctx = Context::new(query, variables).unwrap();
        assert!(validate_sql_query(&ctx).is_ok());
    }

    #[test]
    fn test_multi_selection_set_not_sql() {
        let query = r#"
            query {
                sql(input: { query: "SELECT * FROM users" }) {
                    id
                    name
                }
                users {
                    id
                    name
                }
            }
        "#;
        let variables = r#"{}"#;
        let ctx = Context::new(query, variables).unwrap();
        assert!(validate_sql_query(&ctx).is_err());
    }

    #[test]
    fn test_no_sql() {
        let query = r#"
            query {
                tokens {
                    id
                    name
                }
                users {
                    id
                    name
                }
            }
        "#;
        let variables = r#"{}"#;
        let ctx = Context::new(query, variables).unwrap();
        assert_eq!(
            validate_sql_query(&ctx).err().unwrap().to_string(),
            Error::BadQuery(anyhow!("Fields [tokens, users] are not SQL")).to_string()
        );
    }
}
