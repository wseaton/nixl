# Bring your own buffer to NIXL (and why huge pages make it worth it)

A plain-language walkthrough of one small change to the NIXL Rust bindings,
what it lets you do, and what we measured.

## The 30-second version

- NIXL moves bytes between machines over RDMA. Before it can touch your memory,
  you have to **register** it.
- Registering is slow, and the cost depends on **how many pages** the memory
  is made of, not just how many bytes.
- The Rust bindings had one helper type for host memory, `SystemStorage`, and
  it always made its own `Vec<u8>`. You couldn't hand it memory you allocated
  yourself.
- We made it accept any buffer. Now you can give it memory on **huge pages**,
  and registration gets about **15x faster** for big buffers.

## What "registering memory" means

RDMA lets a network card read and write your process's memory directly, with no
kernel in the middle. For that to be safe, the kernel first has to **pin** every
page (so it can't be swapped or moved) and tell the card where each one lives.

```
   your buffer (4 GiB)
   ┌────┬────┬────┬────┬────┬─────────────┬────┐
   │page│page│page│page│page│     ...     │page│
   └─┬──┴─┬──┴─┬──┴─┬──┴─┬──┴─────────────┴─┬──┘
     │    │    │    │    │                  │
     ▼    ▼    ▼    ▼    ▼                  ▼
   pin  pin  pin  pin  pin      ...       pin     <- one unit of work per page
     │    │    │    │    │                  │
     └────┴────┴────┴────┴────────┬─────────┘
                                  ▼
                        network card's page table
```

The work is per page. So the question that matters is: how many pages?

## Why page size is the whole game

Linux normally hands out memory in 4 KiB pages. It can also use 2 MiB
"huge" pages. Same bytes, far fewer pages.

```
   4 GiB on 4 KiB pages                4 GiB on 2 MiB pages

   ▪▪▪▪▪▪▪▪▪▪▪▪▪▪▪▪▪▪▪▪▪▪▪▪            ██ ██ ██ ██ ██ ██ ██ ██
   ▪▪▪▪▪▪▪▪▪▪▪▪▪▪▪▪▪▪▪▪▪▪▪▪            ██ ██ ██ ██ ██ ██ ██ ██
   ▪▪▪▪▪▪▪▪▪▪▪▪▪▪▪▪▪▪▪▪▪▪▪▪                    ...
            ...

   1,048,576 pages to pin              2,048 pages to pin
```

That's 512 times fewer things to pin. You don't get a 512x speedup (there's
fixed overhead), but you get a lot.

## The problem

Here's what the bindings looked like before:

```rust
pub struct SystemStorage {
    data: Vec<u8>,                       // always a plain Vec
    handle: Option<RegistrationHandle>,
}

impl SystemStorage {
    pub fn new(size: usize) -> Result<Self, NixlError> {
        let data = vec![0; size];        // you get what you get
        Ok(Self { data, handle: None })
    }
}
```

`vec![0; size]` asks the system allocator for memory. On the machine we tested,
that memory came back on 4 KiB pages every time. There was no way to say
"use this buffer I made instead".

Your only other option was to skip `SystemStorage`, implement two traits on
your own type, and keep track of the registration handle yourself. Doable, but
everyone who wants this ends up writing the same wrapper.

## The change

One type got a type parameter. That's most of it.

```rust
pub struct SystemStorage<B = Vec<u8>> {   // B is "whatever buffer you like"
    handle: Option<RegistrationHandle>,
    data: B,
}
```

The `= Vec<u8>` default means old code keeps compiling without edits.

Three new ways in and out:

```rust
// 1. Adopt a Vec you already have
let storage = SystemStorage::from(my_vec);

// 2. Wrap any buffer at all
let storage = unsafe { SystemStorage::from_buffer(my_buffer) };

// 3. Get your buffer back when you're done (this deregisters first)
let my_buffer = storage.into_inner()?;
```

"Any buffer" means anything that can hand out a `&[u8]` and a `&mut [u8]`:

```
                 ┌──────────────────────────────┐
   Vec<u8>  ────▶│                              │
   Vec<u8, A> ──▶│      SystemStorage<B>        │────▶ register with NIXL
   Box<[u8], A> ▶│  (just needs AsRef + AsMut)  │
   your type ───▶│                              │
                 └──────────────────────────────┘
```

### Why is `from_buffer` marked `unsafe`?

Because NIXL writes down your buffer's address once, at registration, and keeps
using it. If the bytes move or get freed afterwards, the network card is now
writing into something else. Rust can't check that for an arbitrary type, so
you have to promise three things:

1. The buffer always points at the same bytes.
2. Those bytes don't move when the buffer value is moved around.
3. Those bytes stay allocated for as long as the storage might be registered.

A `Vec` passes all three (its heap memory stays put when you move the `Vec`).
A plain array like `[u8; 64]` fails number 2, because moving the array moves
the bytes.

### A bug we fixed on the way

Rust drops struct fields top to bottom. The old field order was buffer first,
handle second, which meant:

```
   old order                         new order

   1. free the buffer                1. deregister
   2. deregister        <- oops      2. free the buffer
```

For a moment the memory was freed but still registered. We swapped the fields
so deregistration always comes first, and added a test that fails if anyone
swaps them back.

## Using it with huge pages

Rust 1.100 made custom allocators stable. So you write a small allocator that
asks Linux for huge pages, and give its memory to NIXL.

The allocator, minus error handling (the full version is `MmapAlloc` in
`regbench/src/main.rs`):

```rust
unsafe impl Allocator for HugePageAlloc {
    fn allocate(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
        // 1. mmap a bit more than we need
        // 2. trim it so it starts on a 2 MiB boundary
        // 3. tell the kernel: huge pages here, please
        libc::madvise(base, len, libc::MADV_HUGEPAGE);
        // 4. hand it back
    }

    unsafe fn deallocate(&self, ptr: NonNull<u8>, layout: Layout) {
        libc::munmap(ptr, len);
    }
}
```

And then the part that matters:

```rust
let mut pool = Vec::with_capacity_in(len, HugePageAlloc);
pool.resize(len, 0u8);                        // touch the pages so they exist

let mut storage = unsafe { SystemStorage::from_buffer(pool) };
storage.register(&agent, Some(&opt_args))?;   // fast now
```

Nice detail: NIXL itself never learns about the allocator. It just sees a
buffer. That's why the bindings don't need to require Rust 1.100.

```
   HugePageAlloc ──▶ Vec<u8, HugePageAlloc> ──▶ SystemStorage ──▶ NIXL ──▶ NIC
     (yours)             (std)                   (bindings)

   knows about        knows about               only knows
   huge pages         the allocator             "it's some bytes"
```

## What we measured

One InfiniBand machine, registering buffers of different sizes, median of 10
runs.

| Buffer size | Plain `Vec` (4 KiB pages) | Huge-page buffer | Faster by |
|---|---|---|---|
| 64 MiB | 29 ms | 5.8 ms | 5x |
| 256 MiB | 120 ms | 11 ms | 11x |
| 1 GiB | 612 ms | 34 ms | 18x |
| 4 GiB | 1484 ms | 100 ms | 15x |

```
   time to register 4 GiB

   plain Vec   ████████████████████████████████████████████████  1484 ms
   huge pages  ███                                                100 ms
```

Also worth knowing:

- The plain `Vec` got **zero** huge pages on that machine. The allocator-backed
  one got **100%**.
- Un-registering was an even bigger gap: 421 ms vs 9 ms for 4 GiB.
- No machine setup was needed. This uses "transparent" huge pages, which the
  kernel hands out on request, not a pool someone has to reserve in advance.

## The honest caveats

- **This isn't news to NVIDIA.** Their own benchmark tool has a huge-pages
  flag. What was missing is a way to do it through the Rust helper type.
- **It doesn't help GPU memory.** Real inference servers mostly register GPU
  memory, where host page size doesn't apply. This helps host-memory buffers.
- **If you register once at startup, you save a second, once.** The big wins
  are for buffers that get registered over and over.
- **One machine, one run.** It had 8 network cards and the software registered
  with all of them, so a single-card machine would see smaller absolute numbers.
  We'd expect the ratio to hold but didn't test that.
- **The first touch still costs.** Zeroing 4 GiB of fresh huge pages took about
  900 ms. Huge pages halve that compared with small pages; they don't remove it.

## Where things are

- **The change:** draft PR at https://github.com/wseaton/nixl/pull/1
  (first commit is the bindings change, second is the benchmark).
- **Tests:** 103 passing against the real library on InfiniBand, including a
  real transfer between two custom buffers.
- **Not done yet:** nothing has been proposed to the upstream NIXL project.

## Tiny glossary

| Word | Meaning |
|---|---|
| RDMA | Network cards reading and writing app memory directly, skipping the kernel |
| NIXL | NVIDIA's library for moving inference data around, often over RDMA |
| Register / pin | Lock memory in place and tell the network card where it is |
| Huge page | A 2 MiB memory page instead of the usual 4 KiB |
| THP | Transparent huge pages: the kernel gives you huge pages when you ask nicely |
| Allocator | The thing that decides where a `Vec`'s memory comes from |
