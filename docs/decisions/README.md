# 设计决定记录（ADR）

每个重要设计决定一篇短 ADR，按序号命名：`NNNN-标题.md`。

M0 至少包含：

1. 设备模型选型：为什么只用 virtio-mmio，不引入 PCI / ACPI
2. 启动协议：为什么用 Linux x86 64-bit boot protocol 直接加载 bzImage

（ADR 正文随 M0 Task 2 的实现一同落地。）
