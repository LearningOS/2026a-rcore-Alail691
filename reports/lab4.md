# 实验四（第 6 章）：文件系统与 I/O

## 1. 实验目标

第 6 章引入 virtio-blk 块设备与 easy-fs 文件系统，内核通过 `OSInode`
向用户提供 `openat`、`close`、`read`、`write` 等接口。本实验在此基础上完成：

- `sys_fstat`：获取文件的设备号、inode 号、文件类型与硬链接数；
- `sys_linkat`：为已有文件创建硬链接；
- `sys_unlinkat`：删除目录项，并在链接数归零时回收 inode 与数据块；
- 同时把第 4、5 章的 `get_time`、`mmap/munmap`、`spawn`、优先级调度带到本章。

## 2. 实现思路

### 2.1 在 DiskInode 中增加链接数

easy-fs 原来的 `DiskInode` 没有记录硬链接数量，无法支持 `link/unlink`。
在磁盘 inode 中增加 `nlink` 字段，并在初始化时置 1：

```rust
#[repr(C)]
pub struct DiskInode {
    pub size: u32,
    pub direct: [u32; INODE_DIRECT_COUNT],
    pub indirect1: u32,
    pub indirect2: u32,
    type_: DiskInodeType,
    /// number of hard links pointing to this inode
    pub nlink: u32,
}

impl DiskInode {
    pub fn initialize(&mut self, type_: DiskInodeType) {
        ...
        self.type_ = type_;
        self.nlink = 1;
    }
}
```

由于 easy-fs-fuse 与内核使用同一份 `easy-fs` crate，磁盘镜像与内核解析逻辑
始终保持一致，无需额外迁移。

### 2.2 link 与 unlink

`link(old, new)` 在根目录追加一个目录项，指向旧名字对应的 inode，并把该 inode
的 `nlink` 加一：

```rust
pub fn link(&self, old_name: &str, new_name: &str) -> bool {
    let mut fs = self.fs.lock();
    let old_inode_id = match self.read_disk_inode(|di| self.find_inode_id(old_name, di)) {
        Some(id) => id,
        None => return false,
    };
    if self.read_disk_inode(|di| self.find_inode_id(new_name, di)).is_some() {
        return false;
    }
    self.modify_disk_inode(|root_inode| {
        let file_count = (root_inode.size as usize) / DIRENT_SZ;
        let new_size = (file_count + 1) * DIRENT_SZ;
        self.increase_size(new_size as u32, root_inode, &mut fs);
        let dirent = DirEntry::new(new_name, old_inode_id);
        root_inode.write_at(file_count * DIRENT_SZ, dirent.as_bytes(), &self.block_device);
    });
    // increment the link count of the target inode
    ...
    true
}
```

`unlink(name)` 找到目录项下标后，把其后的目录项整体前移，并缩减目录大小；
随后把目标 inode 的 `nlink` 减一：

```rust
self.modify_disk_inode(|root_inode| {
    let file_count = (root_inode.size as usize) / DIRENT_SZ;
    let mut dirent = DirEntry::empty();
    for i in pos..file_count - 1 {
        root_inode.read_at(DIRENT_SZ * (i + 1), dirent.as_bytes_mut(), &self.block_device);
        root_inode.write_at(DIRENT_SZ * i, dirent.as_bytes(), &self.block_device);
    }
    root_inode.size = ((file_count - 1) * DIRENT_SZ) as u32;
});
```

当 `nlink` 归零时，调用 `clear_size` 释放数据块、把 inode 槽位清零并通过
`dealloc_inode` 归还 inode 位图。这样 `ch6_file3` 中反复
`create → write → unlink` 不会耗尽磁盘空间。

### 2.3 fstat

fd 表保存的是 `Arc<dyn File>`，系统调用层无法直接拿到 `OSInode`，
因此为 `File` trait 增加一个带默认实现的 `stat` 方法：

```rust
pub trait File: Send + Sync {
    ...
    /// get the stat of the file, `None` if it is not a inode-based file
    fn stat(&self) -> Option<Stat> {
        None
    }
}
```

`OSInode` 通过 easy-fs 新增的 `inode_id/nlink/is_dir` 三个方法填写 `Stat`：

```rust
fn stat(&self) -> Option<Stat> {
    let inner = self.inner.exclusive_access();
    let inode = inner.inode.clone();
    Some(Stat {
        dev: 0,
        ino: inode.inode_id() as u64,
        mode: if inode.is_dir() { StatMode::DIR } else { StatMode::FILE },
        nlink: inode.nlink(),
        pad: [0; 7],
    })
}
```

系统调用层再把 `Stat` 通过页表写回用户空间：

```rust
pub fn sys_fstat(fd: usize, st: *mut Stat) -> isize {
    ...
    if let Some(stat) = file.stat() {
        *translated_refmut(token, st) = stat;
        0
    } else {
        -1
    }
}
```

### 2.4 linkat / unlinkat

```rust
pub fn sys_linkat(old_name: *const u8, new_name: *const u8) -> isize {
    let token = current_user_token();
    let old_name = translated_str(token, old_name);
    let new_name = translated_str(token, new_name);
    if ROOT_INODE.link(old_name.as_str(), new_name.as_str()) { 0 } else { -1 }
}

pub fn sys_unlinkat(name: *const u8) -> isize {
    let token = current_user_token();
    let name = translated_str(token, name);
    if ROOT_INODE.unlink(name.as_str()) { 0 } else { -1 }
}
```

## 3. 测试结果

```sh
make -C ci-user test CHAPTER=6
```

测例覆盖：

- `ch6_file0`：创建、写入、关闭、重新打开并读回文件内容；
- `ch6_file1`：`fstat` 返回 `mode == FILE`、`nlink == 1`；
- `ch6_file2`：多次 `link` 后 `nlink` 依次为 2、4，`unlink` 后为 3、1，
  且不同名字打开同一个文件时 `ino` 相同；
- `ch6_file3`：10 轮「创建-写 50 次-删除」循环，验证 inode/数据块回收；
- `ch6b_filetest_simple`、`ch6b_cat`：基础文件读写与 cat。

最终 `check/ch6.py` 全部检查项通过，输出 `ch6 Usertests passed!`。
