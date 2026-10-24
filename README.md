# LiveEarth

<img src="代码/assets/app-icon.png" alt="LiveEarth 图标" width="96">

使用 Rust 开发的 Windows 地球卫星影像壁纸工具。获取 NICT 向日葵卫星影像，拼接完整原图、保存无损归档，并定时更新桌面壁纸。

**当前版本：V2.046。** [下载最新版本](https://github.com/chonger1209/LiveEarth/releases/latest)。

## 功能

- 获取最新影像、指定时刻或历史时间区间的影像。
- 下载服务端 PNG 分片并拼接，完整原图以无损 WebP 保存；兼容已有归档 PNG。
- 支持 550、1100、2200、4400、8800 像素的正方形原图。
- 定时检查、失败重试、任务取消和托盘运行。
- 壁纸缩放 10%–200%；100% 表示原图像素尺寸。
- 按日记录运行日志，显示本次启动的新增与缺失数量。
- 可选数量上限，按观测时间清理最旧归档。
- 设置自动保存，可选择登录 Windows 后自启动。

## 使用

程序适用于 64 位 Windows。EXE 和许可文件放在本仓库的 [Releases](https://github.com/chonger1209/LiveEarth/releases/latest)；源码目录不提交 EXE，也不包含个人照片或设置。

将 `LiveEarth.exe` 和配套许可放入可写目录，双击运行。使用不需要 Rust 或 Python。首次运行不会自动开启定时抓取或开机自启动。

**新配置默认最多保留 99 张。** 在“设置 → 照片保留”中输入 `0` 可关闭数量限制。最新影像新增成功后，超限的最旧归档会被直接删除，不进入回收站。历史任务自身不触发清理，但同目录历史图也会参与后续清理；长期收藏请另存到其他目录。

详细操作见 [使用说明](文档/使用说明.md)，源码编译见 [构建说明](文档/构建说明.md)。

## 影像来源与致谢

- 感谢 **日本气象厅（JMA）**提供向日葵卫星观测数据：[气象卫星介绍](https://www.jma.go.jp/jma/jma-eng/satellite/introduction.html)。
- 感谢 **日本信息通信研究机构（NICT）**提供 [Himawari 实时影像网站](https://himawari8.nict.go.jp/)及数据服务。影像来源署名：**提供：情報通信研究機構（NICT）**。
- 感谢 [EarthLiveSharp](https://github.com/bitdust/EarthLiveSharp) 原作者 **bitdust** 及贡献者。本项目参考其功能与实现思路，采用 Rust 实现并独立维护，不是原项目或上述机构的官方版本，也不表示获得其背书。
- 本软件使用 **MiSans** 字体，感谢小米及字体相关贡献者。保留官方原始字体和[许可协议](代码/assets/fonts/MiSans-License.pdf)。
- 感谢 Rust 生态及 egui/eframe 等开源依赖的维护者，许可汇总见 [第三方声明](THIRD_PARTY_NOTICES.md)。

## 许可与使用范围

LiveEarth 自有源码按 [MIT License](LICENSE) 发布；第三方代码、字体和卫星影像分别适用各自条款，**MIT 不授予卫星影像的使用权**。

NICT 的[影像使用说明](https://himawari8.nict.go.jp/ja/himawari8-about.htm)规定了署名、用途和部分公开使用场景的申请要求。发布、转载或商用影像前，请按实际用途核对该页面；JMA 的通用政策不能代替所用 NICT 服务的具体规则。本仓库不附带卫星照片或包含个人路径的界面截图。

## 当前边界

- 仅维护 Windows x86_64 版本，壁纸按主屏尺寸生成。
- “查看更新”打开 GitHub 最新正式发布页，由你查看版本；不后台请求、不自动比较版本或下载安装。
- 历史影像可用性由源站决定，不保证所有时刻都能下载。
- 睡眠、关机期间不会继续抓取；恢复后检查最新影像，不自动补齐整个离线时段。
