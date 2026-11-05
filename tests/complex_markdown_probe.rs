//! Headless probe for the chat transcript's assistant-markdown path: one long
//! reply that mixes prose, nested lists, tables, Rust/TypeScript/MSSQL code
//! fences and hostile characters (NUL, BEL, ESC, bidi/zero-width controls).
//! It renders through the same read-only, fit-content `LiveEditor` the tile
//! uses, so a parser/layout panic fails `cargo test` without opening the app.

use gpui_kit::gpui::prelude::*;
use gpui_kit::gpui::{
    Context, Entity, IntoElement, Render, TestAppContext, VisualTestContext, Window, div, px, size,
};
use warpui_kit_markdown::{LiveEditor, LiveEditorState};

struct Probe {
    editor: Entity<LiveEditorState>,
}

impl Render for Probe {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let height = self.editor.read(cx).content_height().max(1.);
        // the transcript's assistant bubble: auto width up to the message
        // measure, padding included, then the fit-content editor wrapper
        div().size_full().flex().justify_start().child(
            div().max_w(px(760.)).p_2().px_3().flex().flex_col().child(
                div().w_full().h(px(height)).child(
                    LiveEditor::new(&self.editor)
                        .read_only(true)
                        .fit_content(true)
                        .text_size(px(14.))
                        .code_block_overlay(|block| {
                            Some(
                                div()
                                    .id(gpui_kit::gpui::ElementId::NamedInteger(
                                        "probe-copy".into(),
                                        block.index as u64,
                                    ))
                                    .child("copy")
                                    .into_any_element(),
                            )
                        }),
                ),
            ),
        )
    }
}

/// Characters a model can emit that the transcript never sanitizes: C0/C1
/// controls, a NUL, bidi overrides, zero-width spaces and a BOM.
const HOSTILE: &str = "\u{0}\u{1}\u{7}\u{1b}\u{7f}\u{9b}\u{200b}\u{200e}\u{202e}\u{feff}";

fn complex_reply() -> String {
    let body = r#"## Sonuç

`sp_GetOrderSummary` için öneri aşağıda. **Özet:** `NULL` kontrolü, `WITH (NOLOCK)` ve
parametre doğrulaması eklendi. Satır içi kod: `EXEC dbo.sp_GetOrderSummary @OrderId = 42`.

### Yapılanlar

1. Parametre tipi `INT` → `BIGINT` yapıldı.
2. Sipariş durumu `N'İptal'` / `N'Tamamlandı'` olarak filtrelendi.
   - Alt madde: `[dbo].[OrderDetails]` üzerinde indeks kullanılıyor.
   - Alt madde: `ISNULL(@total, 0)` ile boş sonuç kapatıldı.
3. Hata durumunda `THROW 50001, N'Sipariş bulunamadı', 1;`.

> **Not:** `GO` yalnızca SSMS/`sqlcmd` ayırıcısıdır; SQL Server motoru bunu görmez.
> Aynı betikte iki kez `GO` kullanmak istemci tarafında iki batch üretir.

| Alan | Tip | Null | Açıklama |
| --- | --- | --- | --- |
| `OrderId` | `BIGINT` | Hayır | Birincil anahtar |
| `CustomerId` | `UNIQUEIDENTIFIER` | Hayır | Müşteri |
| `Total` | `DECIMAL(18,2)` | Evet | Toplam tutar |

### Rust tarafı

```rust
pub fn fold_totals<T, U>(items: &[T], mut f: impl FnMut(&T) -> U) -> Vec<U>
where
    T: std::fmt::Debug,
{
    items.iter().map(|item| f(item)).collect()
}
```

### MSSQL

```sql
CREATE OR ALTER PROCEDURE dbo.sp_GetOrderSummary
    @OrderId BIGINT
AS
BEGIN
    SET NOCOUNT ON;

    IF @OrderId IS NULL OR @OrderId <= 0
        THROW 50001, N'Geçersiz sipariş kimliği', 1;

    SELECT o.OrderId,
           o.CustomerId,
           ISNULL(SUM(od.Quantity * od.UnitPrice), 0) AS Total
    FROM [dbo].[Orders] AS o WITH (NOLOCK)
    LEFT JOIN [dbo].[OrderDetails] AS od WITH (NOLOCK)
           ON od.OrderId = o.OrderId
    WHERE o.OrderId = @OrderId
      AND o.[Status] <> N'İptal'
    GROUP BY o.OrderId, o.CustomerId;
END;
GO
```

### TypeScript

```ts
export async function loadSummary(orderId: number): Promise<Summary | null> {
  const response = await fetch(`/api/orders/${orderId}`, { headers: { accept: "application/json" } });
  if (!response.ok) throw new Error(`HTTP ${response.status}: ${await response.text()}`);
  return (await response.json()) as Summary;
}
```

### Kod içinde kontrol karakterleri

```rust
// {HOSTILE}
fn main() {
    let s = "ok"; // {HOSTILE}
    println!("{s}");
}
```

```mssql
SELECT 1; -- {HOSTILE}
```

`{HOSTILE}` satır içi.

### Kenar durumları

- Çok uzun kesintisiz belirteç: `{LONG}`
- Bağlantı: <https://example.com/a?b=c&d=e#frag>
- Birleşik karakterler: Z̴̢̛a̷̧̛l̸̢̛ģ̷̛ơ̵̧
- Emoji: 👩‍👩‍👧‍👦 🚀 ✅
- Bozuk markdown: **kapanmayan kalın, `kapanmayan kod, [kapanmayan bağlantı, <!-- yorum
- Kontrol karakterleri: {HOSTILE}
- Ters bölme ve kaçışlar: \\ \* {HOSTILE} \u0000
- Sayfa sonu: {HOSTILE}
"#;
    body.replace("{HOSTILE}", HOSTILE)
        .replace("{LONG}", &"A".repeat(2000))
}

#[gpui_kit::gpui::test]
fn a_complex_assistant_reply_lays_out(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let text = complex_reply();
    assert!(text.len() > 2000, "probe reply should be substantial");
    let mut editor = None;
    let handle = cx.open_window(size(px(760.), px(1400.)), |window, cx| {
        let state = cx.new(|cx| {
            let mut state = LiveEditorState::new(window, cx);
            state.set_read_only(true, cx);
            state.set_fit_content(true, cx);
            state.set_value(text.clone(), window, cx);
            state
        });
        editor = Some(state.clone());
        Probe { editor: state }
    });
    let cx = VisualTestContext::from_window(handle.into(), cx).into_mut();
    cx.run_until_parked();
    cx.update(|window, cx| drop(window.draw(cx)));
    cx.run_until_parked();
    cx.update(|window, cx| drop(window.draw(cx)));
    let editor = editor.expect("the window was built");
    let height = cx.read(|cx| editor.read(cx).content_height());
    assert!(height > 200., "reply laid out at only {height}px");
}

/// The same reply as it arrives from the server: `set_value` grows while the
/// stream runs, so the parser sees every intermediate shape (open fences,
/// half a table, a control character split across frames). A one-shot parse
/// would miss those states.
#[gpui_kit::gpui::test]
fn a_streaming_reply_lays_out(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let text = complex_reply();
    let mut editor = None;
    let handle = cx.open_window(size(px(760.), px(1400.)), |window, cx| {
        let state = cx.new(|cx| {
            let mut state = LiveEditorState::new(window, cx);
            state.set_read_only(true, cx);
            state.set_fit_content(true, cx);
            state.set_value(String::new(), window, cx);
            state
        });
        editor = Some(state.clone());
        Probe { editor: state }
    });
    let cx = VisualTestContext::from_window(handle.into(), cx).into_mut();
    let editor = editor.expect("the window was built");
    let chars: Vec<char> = text.chars().collect();
    let mut shown = String::new();
    for (index, ch) in chars.iter().enumerate() {
        shown.push(*ch);
        if index % 31 == 0 {
            let chunk = shown.clone();
            cx.update(|window, cx| {
                editor.update(cx, |state, cx| state.set_value(chunk, window, cx));
            });
        }
        if index % 311 == 0 {
            cx.update(|window, cx| drop(window.draw(cx)));
        }
    }
    cx.update(|window, cx| {
        editor.update(cx, |state, cx| state.set_value(text, window, cx));
    });
    cx.run_until_parked();
    cx.update(|window, cx| drop(window.draw(cx)));
    let height = cx.read(|cx| editor.read(cx).content_height());
    assert!(height > 200., "streamed reply laid out at only {height}px");
}
