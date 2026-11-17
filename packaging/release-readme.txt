supfast 1.0.0 — 把 ASS / SSA 字幕按画面变化切段，只渲染必要的帧
========================================================================

绿色免安装。解压到任意目录（路径不要含特殊符号也行，但建议短路径），
双击 supfast.exe 即可运行。


第一次用
--------
界面启动时是空的，没有任何历史。点「选择文件」选一个字幕，或者直接把
.ass / .ssa 拖进窗口。选定后会自动分析，显示这条片子需要渲染多少帧、
切成多少段。确认输出目录后点「开始转换」。

默认参数：23.976 fps · 1080p · 语言码 zho · 8 路并行。
如果是 25 fps 的 PAL 素材或 DVD，用「预设」里对应的一键套用。


系统要求
--------
· Windows 10 1803 以上 / Windows 11（64 位）
· WebView2 运行时：Win11 与新版 Win10 已自带。若双击没有任何窗口，
  说明缺它，去微软官网下载「WebView2 Runtime」安装后即可：
  https://developer.microsoft.com/microsoft-edge/webview2/
· 字幕渲染由 mod\ 目录里的 avs2bdnxml + VSFilterMod 完成，这个目录
  不能删、不能挪。整个目录可以拷到别处，但 exe 必须和 mod\ 同级。


数据存在哪
----------
设置、最近打开的字幕、转换历史存在：
    %APPDATA%\supfast-desktop\state.json

想恢复成全新状态（清空历史与设置），关掉程序后删掉这个文件即可，
下次启动会重新用默认值。


目录说明
--------
  supfast.exe         主程序
  WebView2Loader.dll  WebView2 加载器，必须与 exe 同目录
  mod\
    avs2bdnxml.exe    字幕渲染引擎
    csri\
      VSFilterMod.dll 字幕滤镜（支持 \move、\t、\fad 等动态特效）


输出
----
每次转换在输出目录生成：
  xxx.xml   BDN 字幕描述（可给 Scenarist / easyavs2bdnxml 流程用）
  xxx.sup   PGS 字幕流（可直接混流进 mkv / 蓝光结构）
  00000.png ...  渲染出的字幕帧
