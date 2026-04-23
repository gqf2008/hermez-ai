//! Hermez Calculator Plugin — evaluate mathematical expressions.
//!
//! Provides a `calc` tool that evaluates arithmetic expressions
//! with support for +, -, *, /, ^, parentheses, and common functions.
//!
//! Build: cargo component build --release

#[allow(warnings)]
mod bindings;

use bindings::exports::hermez::plugin::plugin::Guest;
use bindings::hermez::plugin::host;

struct CalcPlugin;

impl Guest for CalcPlugin {
    fn register() {
        host::log("info", "Calc plugin registered — provides 'calc' tool");
    }

    fn on_session_start(_ctx: String) {}
    fn on_session_end(_ctx: String) {}

    fn handle_tool(name: String, args: String) -> Result<String, String> {
        if name != "calc" {
            return Err(format!("Unknown tool: '{}'", name));
        }

        // Parse JSON args: {"expression": "1 + 2 * 3"}
        let expr = match extract_expression(&args) {
            Some(e) => e,
            None => return Err("Missing 'expression' field in args".to_string()),
        };

        match eval_expression(&expr) {
            Ok(result) => Ok(format!("{{\"result\": {}, \"expression\": \"{}\"}}", result, expr)),
            Err(e) => Err(format!("Evaluation error: {}", e)),
        }
    }
}

fn extract_expression(args: &str) -> Option<String> {
    // Simple JSON extraction — look for "expression":"..."
    let key = "\"expression\"";
    let start = args.find(key)? + key.len();
    let start = args[start..].find('"').map(|i| start + i + 1)?;
    let end = args[start..].find('"').map(|i| start + i)?;
    Some(args[start..end].to_string())
}

fn eval_expression(expr: &str) -> Result<f64, String> {
    let tokens = tokenize(expr)?;
    let mut parser = Parser::new(&tokens);
    parser.parse_expr().map(|v| v)
}

#[derive(Debug, Clone, PartialEq)]
enum Token {
    Number(f64),
    Plus,
    Minus,
    Mul,
    Div,
    Pow,
    LParen,
    RParen,
    Ident(String),
}

fn tokenize(input: &str) -> Result<Vec<Token>, String> {
    let mut tokens = Vec::new();
    let chars: Vec<char> = input.chars().collect();
    let mut i = 0;

    while i < chars.len() {
        let c = chars[i];

        if c.is_whitespace() {
            i += 1;
            continue;
        }

        if c.is_ascii_digit() || c == '.' {
            let mut s = String::new();
            while i < chars.len() && (chars[i].is_ascii_digit() || chars[i] == '.') {
                s.push(chars[i]);
                i += 1;
            }
            match s.parse::<f64>() {
                Ok(n) => tokens.push(Token::Number(n)),
                Err(_) => return Err(format!("Invalid number: {}", s)),
            }
            continue;
        }

        if c.is_alphabetic() {
            let mut s = String::new();
            while i < chars.len() && chars[i].is_alphanumeric() {
                s.push(chars[i]);
                i += 1;
            }
            tokens.push(Token::Ident(s));
            continue;
        }

        match c {
            '+' => tokens.push(Token::Plus),
            '-' => tokens.push(Token::Minus),
            '*' => tokens.push(Token::Mul),
            '/' => tokens.push(Token::Div),
            '^' => tokens.push(Token::Pow),
            '(' => tokens.push(Token::LParen),
            ')' => tokens.push(Token::RParen),
            _ => return Err(format!("Unknown character: {}", c)),
        }
        i += 1;
    }

    Ok(tokens)
}

struct Parser<'a> {
    tokens: &'a [Token],
    pos: usize,
}

impl<'a> Parser<'a> {
    fn new(tokens: &'a [Token]) -> Self {
        Self { tokens, pos: 0 }
    }

    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.pos)
    }

    fn consume(&mut self) -> Option<&Token> {
        let t = self.tokens.get(self.pos);
        if t.is_some() {
            self.pos += 1;
        }
        t
    }

    fn expect(&mut self, expected: Token) -> Result<(), String> {
        match self.consume() {
            Some(t) if *t == expected => Ok(()),
            Some(t) => Err(format!("Expected {:?}, got {:?}", expected, t)),
            None => Err(format!("Expected {:?}, got EOF", expected)),
        }
    }

    fn parse_expr(&mut self) -> Result<f64, String> {
        self.parse_add_sub()
    }

    fn parse_add_sub(&mut self) -> Result<f64, String> {
        let mut left = self.parse_mul_div()?;
        while let Some(t) = self.peek() {
            match t {
                Token::Plus => {
                    self.consume();
                    left += self.parse_mul_div()?;
                }
                Token::Minus => {
                    self.consume();
                    left -= self.parse_mul_div()?;
                }
                _ => break,
            }
        }
        Ok(left)
    }

    fn parse_mul_div(&mut self) -> Result<f64, String> {
        let mut left = self.parse_power()?;
        while let Some(t) = self.peek() {
            match t {
                Token::Mul => {
                    self.consume();
                    left *= self.parse_power()?;
                }
                Token::Div => {
                    self.consume();
                    let right = self.parse_power()?;
                    if right == 0.0 {
                        return Err("Division by zero".to_string());
                    }
                    left /= right;
                }
                _ => break,
            }
        }
        Ok(left)
    }

    fn parse_power(&mut self) -> Result<f64, String> {
        let mut left = self.parse_unary()?;
        while let Some(t) = self.peek() {
            match t {
                Token::Pow => {
                    self.consume();
                    left = left.powf(self.parse_unary()?);
                }
                _ => break,
            }
        }
        Ok(left)
    }

    fn parse_unary(&mut self) -> Result<f64, String> {
        match self.peek() {
            Some(Token::Minus) => {
                self.consume();
                Ok(-self.parse_unary()?)
            }
            Some(Token::Plus) => {
                self.consume();
                self.parse_unary()
            }
            _ => self.parse_primary(),
        }
    }

    fn parse_primary(&mut self) -> Result<f64, String> {
        match self.peek() {
            Some(Token::Number(n)) => {
                let v = *n;
                self.consume();
                Ok(v)
            }
            Some(Token::Ident(name)) => {
                let name = name.clone();
                self.consume();
                match name.as_str() {
                    "pi" => Ok(std::f64::consts::PI),
                    "e" => Ok(std::f64::consts::E),
                    "sqrt" | "sin" | "cos" | "tan" | "log" | "ln" | "abs" | "floor" | "ceil" | "round" => {
                        self.expect(Token::LParen)?;
                        let arg = self.parse_expr()?;
                        self.expect(Token::RParen)?;
                        Ok(apply_function(&name, arg))
                    }
                    _ => Err(format!("Unknown identifier: {}", name)),
                }
            }
            Some(Token::LParen) => {
                self.consume();
                let v = self.parse_expr()?;
                self.expect(Token::RParen)?;
                Ok(v)
            }
            Some(t) => Err(format!("Unexpected token: {:?}", t)),
            None => Err("Unexpected EOF".to_string()),
        }
    }
}

fn apply_function(name: &str, arg: f64) -> f64 {
    match name {
        "sqrt" => arg.sqrt(),
        "sin" => arg.sin(),
        "cos" => arg.cos(),
        "tan" => arg.tan(),
        "log" => arg.log10(),
        "ln" => arg.ln(),
        "abs" => arg.abs(),
        "floor" => arg.floor(),
        "ceil" => arg.ceil(),
        "round" => arg.round(),
        _ => arg,
    }
}

bindings::export!(CalcPlugin with_types_in bindings);
