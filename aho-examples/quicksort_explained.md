# Understanding Quicksort

## Overview
Quicksort is a highly efficient **divide-and-conquer** sorting algorithm invented by Tony Hoare in 1959. It's one of the most widely used sorting algorithms due to its excellent average-case performance.

## How It Works

### Core Concept
1. **Pick a pivot**: Choose an element from the array
2. **Partition**: Rearrange the array so that:
   - All elements smaller than the pivot come before it
   - All elements greater than the pivot come after it
3. **Recursively sort**: Apply the same process to the sub-arrays on either side of the pivot

### Step-by-Step Example

Let's sort `[8, 3, 1, 7, 0, 10, 2]`:

```
Initial: [8, 3, 1, 7, 0, 10, 2]
         Pick pivot = 2 (last element)

Partition around 2:
  [1, 0] | 2 | [8, 3, 7, 10]
  smaller      larger

Recursively sort left side [1, 0]:
  [0] | 1 | []
  Result: [0, 1]

Recursively sort right side [8, 3, 7, 10]:
  Pick pivot = 10
  [8, 3, 7] | 10 | []
  
  Sort [8, 3, 7]:
    Pick pivot = 7
    [3] | 7 | [8]
    Result: [3, 7, 8]
  
  Result: [3, 7, 8, 10]

Final: [0, 1, 2, 3, 7, 8, 10] ✓
```

## Time Complexity

- **Best case**: O(n log n) - pivot always divides array evenly
- **Average case**: O(n log n) - typical random data
- **Worst case**: O(n²) - pivot is always smallest/largest (already sorted array)

## Space Complexity

- **O(log n)** - recursive call stack for average case
- **O(n)** - worst case call stack depth

## Advantages

✅ Very fast in practice (average case)  
✅ In-place sorting (minimal extra memory)  
✅ Cache-efficient (good locality of reference)  
✅ Parallelizable

## Disadvantages

❌ Worst case O(n²) performance  
❌ Not stable (doesn't preserve order of equal elements)  
❌ Recursive (stack overflow risk on large arrays)

## Optimizations

1. **Random pivot selection**: Avoid worst case on sorted data
2. **Median-of-three**: Choose median of first, middle, last elements
3. **Insertion sort for small arrays**: Switch to insertion sort when partition size < 10
4. **Three-way partitioning**: Handle duplicate values efficiently
