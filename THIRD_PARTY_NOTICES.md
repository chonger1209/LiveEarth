# 第三方来源与许可

## 卫星影像：JMA 与 NICT

向日葵卫星观测数据来自日本气象厅（JMA），本程序从 NICT Himawari 实时影像服务获取分片。

**提供：情報通信研究機構（NICT）**

- 服务：https://himawari8.nict.go.jp/
- NICT 影像使用说明：https://himawari8.nict.go.jp/ja/himawari8-about.htm
- JMA 气象卫星中心使用条款：https://www.data.jma.go.jp/mscweb/en/general/note.html

核对日期：2026-10-24。NICT 页面规定非营利用途限制，同时列有互联网、出版等用途及部分申请流程；本项目不把这些条款概括为“任意免费商用”。实际用途应以其现行条款为准。软件的 MIT 许可证不覆盖下载影像，本源码仓库不分发卫星图片。

## EarthLiveSharp

参考项目：https://github.com/bitdust/EarthLiveSharp

感谢 bitdust 及贡献者的功能和实现参考。原项目采用 MIT，保留[原版权声明及许可全文](许可证/EarthLiveSharp-MIT.txt)。本项目独立维护，非官方续作；更换语言本身不构成“全部原创”的证明。

## MiSans

软件使用官方静态 MiSans Regular，未修改、未裁剪。

原字体版权：Copyright © 2020-2025 Beijing Xiaomi Mobile Software Co.,Ltd. All Rights Reserved.

来源、文件摘要及许可见[字体说明](代码/assets/fonts/README.md)和[官方许可 PDF](代码/assets/fonts/MiSans-License.pdf)。分发 EXE 时应随附该许可与字体说明，不将字体单独作为商品分发。

## Rust 依赖与内嵌回退字体

版本以 `代码/Cargo.lock` 为准。[Rust 依赖许可汇总](许可证/Rust依赖许可.txt)保存 Windows GNU 目标解析到的 206 项依赖的许可声明及可取得的许可全文（含构建依赖）。缓存包缺少许可正文时从其 `.cargo_vcs_info.json` 对应的上游提交取得，未用当前主分支内容冒充发布版本。

egui 自带的 Hack、Noto Emoji、Ubuntu 和 emoji-icon-font 字体具有独立条款，相关正文包含在上述汇总的 epaint_default_fonts 段落。依赖许可不被项目 MIT 替代。

本文件说明已识别的来源与分发材料，不表示 JMA、NICT、EarthLiveSharp 或字体作者认可本项目。
