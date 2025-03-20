use crate::sql::SqlDocParser;
use colored::Colorize;

pub fn run(args: &[String]) -> Result<(), String> {
    if args.is_empty() {
        return print_all_commands();
    }
    
    match args[0].as_str() {
        "list" => {
            return print_all_commands();
        },
        "explain" => {
            if args.len() < 2 {
                println!("Please provide a SQL statement to explain");
                return print_all_commands();
            }
            
            let query = &args[1..].join(" ");
            return explain_query(query);
        },
        _ => {
            // Assume the first argument is a SQL statement to explain
            let query = &args.join(" ");
            return explain_query(query);
        }
    }
}

fn print_all_commands() -> Result<(), String> {
    println!("{}", "Supported SQL Commands:".bold().green());
    println!();
    
    // Group by category for better readability
    let mut schema_cmds = Vec::new();
    let mut pipeline_cmds = Vec::new();
    let mut data_cmds = Vec::new();
    let mut query_cmds = Vec::new();
    
    for doc in SqlDocParser::list_all_statements() {
        let cmd = format!("{}: {}", doc.name, doc.description);
        
        if doc.name.contains("SCHEMA") {
            schema_cmds.push(cmd);
        } else if doc.name.contains("PIPELINE") {
            pipeline_cmds.push(cmd);
        } else if doc.name.contains("TABLE") || doc.name.contains("DATABASE") {
            data_cmds.push(cmd);
        } else {
            query_cmds.push(cmd);
        }
    }
    
    if !schema_cmds.is_empty() {
        println!("{}", "Schema Operations:".bold().blue());
        for cmd in schema_cmds {
            println!("  {}", cmd);
        }
        println!();
    }
    
    if !pipeline_cmds.is_empty() {
        println!("{}", "Pipeline Operations:".bold().blue());
        for cmd in pipeline_cmds {
            println!("  {}", cmd);
        }
        println!();
    }
    
    if !data_cmds.is_empty() {
        println!("{}", "Data Operations:".bold().blue());
        for cmd in data_cmds {
            println!("  {}", cmd);
        }
        println!();
    }
    
    if !query_cmds.is_empty() {
        println!("{}", "Query Operations:".bold().blue());
        for cmd in query_cmds {
            println!("  {}", cmd);
        }
        println!();
    }
    
    println!("For more details on a specific command, use: skippr sql-help explain \"<YOUR SQL COMMAND>\"");
    
    Ok(())
}

fn explain_query(query: &str) -> Result<(), String> {
    match SqlDocParser::parse_and_document(query) {
        Ok(Some(doc)) => {
            println!("{}: {}", "SQL Command".bold().green(), doc.name.bold());
            println!();
            println!("{}: {}", "Description".bold(), doc.description);
            println!();
            println!("{}: ", "Syntax".bold());
            println!("{}", doc.syntax.blue());
            println!();
            println!("{}: ", "Example".bold());
            println!("{}", doc.example.blue());
            
            Ok(())
        },
        Ok(None) => {
            println!("{}", "Unknown SQL command or standard SQL query.".yellow());
            println!("If this is a standard SQL query, it may be supported by the system but not specifically documented.");
            println!();
            println!("For a list of documented SQL commands, use: skippr sql-help list");
            
            Ok(())
        },
        Err(e) => {
            Err(format!("Error: {}", e))
        }
    }
} 