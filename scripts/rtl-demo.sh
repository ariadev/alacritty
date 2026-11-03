#!/usr/bin/env bash
# Run inside the experimental build with docs/rtl/alacritty.toml.
printf '%s\n' \
  'Persian: سلام دنیا — فارسی را درست بخوانیم' \
  'Arabic: السلام عليكم ورحمة الله' \
  'Mixed: version 1.2 — نسخهٔ ۱۲۳ — abc@example.com' \
  'Joining: ب بب ببب | پ چ ژ گ ی ک | لا لآ لأ لإ' \
  'ZWNJ: می‌روم خانه‌ها | ZWJ: ب‍ب | unjoined: ب‌ب' \
  'Marks: بِسْمِ اللَّهِ الرَّحْمَٰنِ الرَّحِيمِ' \
  'Punctuation: (سلام) [دنیا] {۱۲۳} 123 / ۱۲۳ / ١٢٣' \
  'Wide/emoji: فارسی 中文 😀 English' \
  'Copy these lines: clipboard should retain the original logical text.'
printf '\033[31m%s\033[32m%s\033[0m\n' 'فارسی ' 'رنگی'
