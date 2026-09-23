/// Corta textos longos mantendo o começo e (principalmente) o fim.
pub fn clip(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let head = max / 4;
    let tail = max - head;
    let head_end = floor_boundary(s, head);
    let tail_start = ceil_boundary(s, s.len() - tail);
    format!(
        "{}\n\n[... {} bytes omitidos ...]\n\n{}",
        &s[..head_end],
        tail_start - head_end,
        &s[tail_start..]
    )
}

fn floor_boundary(s: &str, mut i: usize) -> usize {
    while !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn ceil_boundary(s: &str, mut i: usize) -> usize {
    while !s.is_char_boundary(i) {
        i += 1;
    }
    i
}

#[cfg(test)]
mod tests {
    use super::clip;

    #[test]
    fn clip_keeps_short() {
        assert_eq!(clip("abc", 10), "abc");
    }

    #[test]
    fn clip_multibyte() {
        let s = "ção".repeat(1000);
        let c = clip(&s, 100);
        assert!(c.contains("omitidos"));
    }
}
