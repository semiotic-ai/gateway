use anyhow::anyhow;
use cost_model::Context;
use gateway_framework::errors::Error;
use graphql::graphql_parser::query::OperationDefinition;

pub fn invalidate_sql_query(ctx: &Context<String>) -> Result<(), Error> {
    if ctx.operations.iter().any(|operation| {
        if let OperationDefinition::Query(query) = operation {
            query.selection_set.items.iter().any(|selection| {
                if let graphql::graphql_parser::query::Selection::Field(field) = selection {
                    return field.name == "sql";
                }
                false
            })
        } else {
            false
        }
    }) {
        return Err(Error::BadQuery(anyhow!("Query contains SQL")));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(invalidate_sql_query(&ctx).is_ok());
    }

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
        assert!(invalidate_sql_query(&ctx).is_err());
    }

    #[test]
    fn test_multi_selection_set_with_sql() {
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
        assert!(invalidate_sql_query(&ctx).is_err());
    }

    #[test]
    fn test_multi_sql_fields() {
        let query = r#"
            query {
                sql(input: { query: "SELECT * FROM tokens" }) {
                    id
                    name
                }
                sql(input: { query: "SELECT * FROM users" }) {
                    id
                    name
                }
            }
        "#;
        let variables = r#"{}"#;
        let ctx = Context::new(query, variables).unwrap();
        assert!(invalidate_sql_query(&ctx).is_err());
    }
}
