# Chuẩn phát hành plugin Aizen Desktop v1

Chuẩn này dành cho seller muốn phát hành plugin có màn hình trong Aizen Desktop. Gói phát hành là một tệp **aizen-release.json**. Bạn có thể dùng gói mẫu để tạo bảng công việc, máy tính hoặc nhiều màn hình trong cùng sản phẩm. Aizen Desktop hiển thị các màn hình bằng các thành phần có sẵn.

Để thử và sử dụng plugin, bạn cần bản Aizen Desktop beta có hỗ trợ plugin v1. [JSON Schema cho trình soạn thảo](aizen-release.schema.json).

## 1. Lấy mẫu và đặt tên

Tải **mẫu plugin Desktop** tại Dashboard → Gian hàng → Plugin → Bản phát hành. Đổi `slug`, `version`, tiêu đề màn hình và tên cột. Dùng cùng slug trong tệp và trong bài nộp Marketplace. Giữ nguyên ID màn hình và ID cột khi bạn chỉ đổi tên hiển thị. Desktop dùng ID để tìm lại dữ liệu đã lưu.

```json
{
  "schemaVersion": 1,
  "slug": "bang-cong-viec-mau",
  "version": "1.0.0",
  "desktop": {
    "uiApiVersion": 1,
    "views": [
      {
        "type": "board",
        "id": "board",
        "title": "Bảng công việc",
        "columns": [
          { "id": "todo", "title": "Cần làm" },
          { "id": "doing", "title": "Đang làm" },
          { "id": "done", "title": "Hoàn tất" }
        ]
      },
      { "type": "calculator", "id": "calculator", "title": "Máy tính" }
    ]
  }
}
```

| Trường | Quy tắc |
| --- | --- |
| `schemaVersion`, `desktop.uiApiVersion` | Số nguyên `1`. |
| `slug`, ID màn hình, ID cột | 2–64 ký tự gồm chữ thường ASCII, số và dấu gạch ngang. Bắt đầu bằng chữ hoặc số. ID màn hình duy nhất trong gói. ID cột duy nhất trong từng bảng. |
| `version` | `X.Y.Z`, tối đa 32 ký tự. Các phần là số nguyên không thêm số 0 đầu. |
| `desktop.views` | 1–6 màn hình theo thứ tự muốn hiển thị. Màn hình đầu tiên mở mặc định. |
| `title` | Tiêu đề có nội dung, tối đa 160 byte UTF-8. |
| `board` | 2–8 cột. Người dùng thêm, đổi tên, di chuyển và xóa thẻ. Mỗi bảng lưu tối đa 128 thẻ trên máy, theo tài khoản và sản phẩm. |
| `calculator` | Hai số đầu vào, bốn phép tính cộng, trừ, nhân và chia. |

Tệp phải là UTF-8, tối đa 32 KiB. Dùng đúng tên trường trong mẫu. Nếu cột bị bỏ ở bản mới, các thẻ của cột đó xuất hiện trong cột đầu tiên. Bạn có thể ghi thay đổi này vào changelog để người dùng dễ theo dõi.

## 2. Kiểm tra rồi mở bản nháp

Với Aizen Core có hỗ trợ chuẩn v1, chạy lệnh sau trong thư mục chứa tệp.

```sh
aizen plugin validate-release ./aizen-release.json
```

Trong Desktop, mở **Marketplace → Dành cho tác giả → Thử bản nháp**. Chọn tệp JSON, mở lần lượt các màn hình và thử thao tác. Thẻ thử chỉ sống trong phiên xem trước. Bạn có thể quay video demo hoặc chụp màn hình ở bước này.

## 3. Gửi duyệt và cập nhật

1. Đăng nhập Dashboard bằng tài khoản seller đã được duyệt. Vào **Gian hàng → Plugin** và tạo sản phẩm.
2. Điền tên, slug, mô tả, giá và kỳ thanh toán. Thêm ảnh, demo hoặc hướng dẫn hỗ trợ ở phần nội dung gian hàng.
3. Chọn **Aizen Desktop**, điền phiên bản khớp với tệp, tải `aizen-release.json` lên và ghi thay đổi. Marketplace tự tính SHA-256 trên tệp, lưu bản nộp và đưa vào hàng chờ.
4. Theo dõi phản hồi của admin trong gian hàng. Với bản đã duyệt, tăng phiên bản để gửi bản mới. Nếu admin trả bài, sửa theo phản hồi rồi nộp lại. Bản đang bán tiếp tục được giao theo quyền tải đã cấp cho khách.
5. Để thử như khách, dùng tài khoản thử khác, nhận hoặc mua trên Aizen Web, rồi đăng nhập cùng tài khoản đó trong Desktop. Vào **Marketplace → Đã cài**, tải danh mục và bấm **Mở plugin**. Mỗi lần mở, Desktop kiểm tra quyền và nhận phiên bản phù hợp. Người có cửa sổ cập nhật giới hạn có thể nhận bản đã duyệt cũ hơn.

## Phạm vi tương thích

Hai loại màn hình v1 dùng bộ hiển thị có sẵn, dữ liệu bảng lưu trên máy. Việc mở bản đã nhận cần kết nối Aizen để kiểm quyền. Giao diện HTML/React tùy biến, dữ liệu đồng bộ giữa máy, thao tác từ chat và phát hành MCP hoặc native qua gói này thuộc các mốc mở rộng.

Plugin Gateway dùng mẫu ZIP JavaScript tương ứng. Native Memory vẫn dùng hợp đồng `plugin.json` của Core và MCP được cấu hình qua mục MCP. Skill tiếp tục dùng gói ZIP chứa `SKILL.md`.
