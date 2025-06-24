use std::error::Error;

type ErrorType = Box<dyn Error + Send + Sync>;

pub fn parse_kv_pair<T, U>(s: &str) -> Result<(T, U), ErrorType>
where
    T: std::str::FromStr,
    T::Err: Error + Send + Sync + 'static,
    U: std::str::FromStr,
    U::Err: Error + Send + Sync + 'static,
{
    let pos = s
        .find(':')
        .ok_or_else(|| format!("Invalid KEY:VALUE: no `:` found in `{s}`"))?;
    Ok((s[..pos].parse()?, s[pos + 1..].parse()?))
}
