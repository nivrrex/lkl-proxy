# lkl-proxy

## 介绍

lkl-proxy 是专为 OpenVZ 上 LKL 端口转发编写的 HAProxy 替代品，Rust 编写，零依赖，做 TCP 四层转发。

起因很直接：OpenVZ无法定制内核，HAProxy 跑在宿主内核，拥塞控制只能用 cubic，而 BBRplus 只存在于 LKL 用户态协议栈里。要让 443 入口流量吃上 BBRplus，转发进程必须挂在 `LD_PRELOAD=liblkl-hijack.so` 下运行，HAProxy 最新版本已经在 openvz 上不可用，所以自己写了一个。

## 具体

- 不用异步运行时。tokio/mio 建的监听在 hijack 下 accept 得 EBADF，epoll 注册落在未经 hook 创建的 epfd 上（spy-probe 直接目击）；io_uring 更不行，hijack 没有对应 hook，注册 fd 得 EBADF，OpenVZ 容器里 `io_uring_setup` 直接 ENOSYS。只保留 hijack 验证过的经典阻塞调用：socket/bind/listen/accept/connect/read/write。
- 不用 splice。功能是通的，但 hijack 下实测只有 read/write copy 一半速度（回环 8MB：copy 约 1000MB/s，splice 约 500MB/s，三轮一致），零拷贝收益没兑现。
- 纯 std，零 crate 依赖，`cargo build --release` 可离线完成。一条连接一组转发线程，两个方向各跑 `io::copy`，单向读到 EOF 就对端 shutdown write，半关闭正常走完；超时用 socket 选项设置，在 fd clone 之前做，dup 出的 fd 共享同一 socket。
- 配置外置，`KEY=VALUE` 文件（默认 `/etc/lkl-haproxy/lkl-proxy.conf`），默认值对齐原 haproxy.cfg：

  | 键 | 默认 | 含义 |
  |---|---|---|
  | `LISTEN` | `0.0.0.0:443` | 入口监听 |
  | `BACKEND` | `10.0.0.1:443` | 后端地址 |
  | `MAXCONN` | `20480` | 最大并发，超出直接拒绝 |
  | `CONNECT_TIMEOUT_MS` | `5000` | 后端连接超时 |
  | `RW_TIMEOUT_MS` | `50000` | 读写超时 |

  优先级：环境变量 > 配置文件 > 内置默认；`--config/-c` 或 `$LKL_PROXY_CONFIG` 改路径；文件缺失回退纯环境变量行为，值非法直接退出，启动日志打印每项取值来源。
- 两条硬约束：配置必须在任何 socket 调用之前读完（hijack 只拦 socket，普通文件 IO 走宿主）；地址一律数字 IP，hijack 下 DNS 不可用。

## 构建与运行

```bash
cargo build --release
```

service 每次启动先重建 TAP（destroy/init），再以 `LD_PRELOAD` 拉起代理，就绪判定是 ping 10.0.0.2——bind 成功不代表网络通。回滚：停服后拷回 `.bak` 再 start。


## 备注

- 不计划增加 HTTP/TLS 七层解析、负载策略、热重载等功能，因为定位就是 LKL 栈内的四层直通入口，功能越多，在 hijack 受限环境里的出错面越大；五个配置键已对齐原 haproxy，够用。
- 不计划引入异步运行时和 splice，因为上面两条都是实测结论（不可用 / 无收益），不是风格取舍。
- `lib/liblkl-hijack.so` 下载使用详见 https://github.com/nivrrex/lkl-bbr 。
