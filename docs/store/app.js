// RCGUI 插件商店页面。
//
// 只做三件事：读同目录下的 index.json、按关键字过滤、把每条渲染成卡片。
// 没有框架、没有构建、没有网络请求之外的依赖，也不加载任何统计脚本。
//
// 数据全部来自 index.json，用 textContent 写进 DOM，不用 innerHTML 拼字符串：
// 索引是仓库里的文件，但拼 HTML 的写法一旦出现在页面里，迟早会有人把
// 插件描述直接插进去，那时就是注入问题。

(function () {
  "use strict";

  var SUPPORTED_SCHEMA = 1;

  // 权限的中文解释。插件声明了哪几项，页面就把哪几项翻成人话——
  // 只说"fs"没人知道意味着什么，"读写你本来就能读写的文件"才说得清代价。
  // 这里的措辞必须和本体实际拦得住的东西一致：fs 的 broker 并不按路径判权限，
  // 所以不能写成"只能读被允许的目录"；rclone 能改能删远程上的数据，就得直说。
  var CAPABILITY_TEXT = {
    kv: { label: "kv", title: "存自己的设置（键值存储），不碰别的插件的数据" },
    ui: { label: "ui", title: "在界面里加面板、按钮或设置项" },
    fs: { label: "fs", title: "读写本地文件：范围是这个用户本来就能读写的文件，没有按目录另做限制", risky: true },
    http: { label: "http", title: "发 HTTP 请求：能把数据发到外网服务，也能从外网取数据", risky: true },
    proc: { label: "proc", title: "启动子进程：能在本机跑别的程序", risky: true },
    net: { label: "net", title: "开监听端口或做网络连接", risky: true },
    rclone: { label: "rclone", title: "调用 rclone 的远程操作：能上传，也能改和删你远程上的数据", risky: true },
    env: { label: "env", title: "读环境变量" }
  };

  var listEl = document.getElementById("cards");
  var countEl = document.getElementById("count");
  var searchEl = document.getElementById("search");
  var noticeEl = document.getElementById("notice");

  var plugins = [];

  function showNotice(html) {
    if (!noticeEl) return;
    noticeEl.hidden = false;
    noticeEl.textContent = html;
  }

  function el(tag, className, text) {
    var node = document.createElement(tag);
    if (className) node.className = className;
    if (text !== undefined && text !== null) node.textContent = text;
    return node;
  }

  function renderCaps(caps) {
    var wrap = el("div", "caps");
    (caps || []).forEach(function (name) {
      var info = CAPABILITY_TEXT[name] || { label: name, title: "未在页面里登记说明的权限，请查看插件仓库" };
      var chip = el("span", "cap" + (info.risky ? " risky" : ""), info.label);
      chip.title = info.title;
      // title 只对鼠标有效，键盘用户需要能聚焦看到同样的说明。
      chip.tabIndex = 0;
      wrap.appendChild(chip);
    });
    return wrap;
  }

  function copyButton(text) {
    var btn = el("button", null, "复制 sha256");
    btn.type = "button";
    btn.addEventListener("click", function () {
      var done = function () {
        btn.textContent = "已复制";
        setTimeout(function () { btn.textContent = "复制 sha256"; }, 1500);
      };
      if (navigator.clipboard && navigator.clipboard.writeText) {
        navigator.clipboard.writeText(text).then(done, function () {
          btn.textContent = "复制失败，请手动选中";
        });
      } else {
        btn.textContent = "浏览器不支持复制，请手动选中";
      }
    });
    return btn;
  }

  function card(plugin) {
    var li = el("li", "card");

    var h2 = el("h2");
    h2.appendChild(el("span", null, plugin.name || plugin.id));
    h2.appendChild(el("span", "id", plugin.id));
    li.appendChild(h2);

    li.appendChild(el("p", "desc", plugin.description || ""));

    li.appendChild(renderCaps(plugin.capabilities));

    var meta = el("div", "meta");
    meta.appendChild(el("span", null, "版本 " + plugin.version));
    meta.appendChild(el("span", null, "需要 RCGUI " + plugin.min_gui_version + " 及以上"));
    li.appendChild(meta);

    // 哈希放进可展开区：它是核对用的，不该和功能说明抢同一屏的位置。
    // 用 <details> 而不是自己写 toggle，键盘和屏幕阅读器都直接能用。
    var details = el("details", "hash-details");
    details.appendChild(el("summary", null, "sha256 与来源"));
    details.appendChild(el("div", "hash", plugin.sha256));

    var links = el("div", "hash-links");
    var repo = el("a", null, "源码仓库");
    repo.href = plugin.repository;
    repo.rel = "noopener";
    links.appendChild(repo);
    links.appendChild(copyButton(plugin.sha256));
    details.appendChild(links);
    li.appendChild(details);

    var actions = el("div", "actions");
    var download = el("a", "primary", "下载 zip");
    download.href = plugin.url;
    download.rel = "noopener";
    actions.appendChild(download);
    li.appendChild(actions);

    return li;
  }

  function matches(plugin, query) {
    if (!query) return true;
    var hay = [
      plugin.id,
      plugin.name,
      plugin.description,
      plugin.version,
      (plugin.capabilities || []).join(" ")
    ].join(" ").toLowerCase();
    return hay.indexOf(query) !== -1;
  }

  function render() {
    var query = (searchEl && searchEl.value ? searchEl.value : "").trim().toLowerCase();
    var shown = plugins.filter(function (p) { return matches(p, query); });

    listEl.textContent = "";
    if (shown.length === 0) {
      var empty = el("li", "empty", plugins.length === 0
        ? "索引里还没有插件。"
        : "没有匹配“" + query + "”的插件。试试插件名、id 或权限名。");
      listEl.appendChild(empty);
    } else {
      shown.forEach(function (p) { listEl.appendChild(card(p)); });
    }

    if (countEl) {
      countEl.textContent = "共 " + plugins.length + " 个插件" +
        (query ? "，匹配 " + shown.length + " 个" : "");
    }
  }

  function load() {
    // 用相对路径：GitHub Pages 上页面在 /store/，index.json 就在旁边。
    fetch("index.json", { cache: "no-cache" })
      .then(function (res) {
        if (!res.ok) throw new Error("HTTP " + res.status);
        return res.json();
      })
      .then(function (data) {
        if (!data || data.schema_version !== SUPPORTED_SCHEMA) {
          showNotice("这个页面的脚本只认索引 schema_version " + SUPPORTED_SCHEMA +
            "，读到的是 " + (data && data.schema_version) + "。页面需要更新。");
          return;
        }
        plugins = Array.isArray(data.plugins) ? data.plugins : [];
        render();
      })
      .catch(function (err) {
        showNotice("读不到 index.json（" + err.message + "）。" +
          "直接双击打开本地文件时浏览器会阻止读取，请通过 http 服务打开，或换用发布在 GitHub Pages 上的页面。");
      });
  }

  if (searchEl) {
    searchEl.addEventListener("input", render);
  }

  load();
})();
