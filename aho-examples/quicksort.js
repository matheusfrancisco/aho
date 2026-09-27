/**
 * Quicksort Implementations in JavaScript
 * Demonstrates different variations of the quicksort algorithm
 */

// ============================================================================
// 1. Classic Quicksort (In-place, Lomuto partition)
// ============================================================================

function quicksortLomuto(arr, low = 0, high = arr.length - 1) {
  /**
   * In-place quicksort using Lomuto partition scheme.
   * Time: O(n log n) average, O(n²) worst
   * Space: O(log n) for recursion stack
   */
  if (low < high) {
    const pivotIdx = partitionLomuto(arr, low, high);
    quicksortLomuto(arr, low, pivotIdx - 1);
    quicksortLomuto(arr, pivotIdx + 1, high);
  }
  return arr;
}

function partitionLomuto(arr, low, high) {
  /**
   * Lomuto partition: pivot is last element.
   */
  const pivot = arr[high];
  let i = low - 1;

  for (let j = low; j < high; j++) {
    if (arr[j] <= pivot) {
      i++;
      [arr[i], arr[j]] = [arr[j], arr[i]]; // Swap
    }
  }

  [arr[i + 1], arr[high]] = [arr[high], arr[i + 1]]; // Place pivot
  return i + 1;
}

// ============================================================================
// 2. Hoare Partition Scheme (Original, more efficient)
// ============================================================================

function quicksortHoare(arr, low = 0, high = arr.length - 1) {
  /**
   * In-place quicksort using Hoare partition scheme.
   * Generally more efficient than Lomuto.
   */
  if (low < high) {
    const pivotIdx = partitionHoare(arr, low, high);
    quicksortHoare(arr, low, pivotIdx);
    quicksortHoare(arr, pivotIdx + 1, high);
  }
  return arr;
}

function partitionHoare(arr, low, high) {
  /**
   * Hoare partition: more efficient (fewer swaps).
   */
  const pivot = arr[low];
  let i = low - 1;
  let j = high + 1;

  while (true) {
    do { i++; } while (arr[i] < pivot);
    do { j--; } while (arr[j] > pivot);

    if (i >= j) return j;

    [arr[i], arr[j]] = [arr[j], arr[i]]; // Swap
  }
}

// ============================================================================
// 3. Functional/Immutable Quicksort (Pure function)
// ============================================================================

function quicksortFunctional(arr) {
  /**
   * Functional quicksort: returns new array, doesn't modify original.
   * More intuitive but uses more memory.
   * 
   * Time: O(n log n) average
   * Space: O(n) - creates new arrays
   */
  if (arr.length <= 1) {
    return arr;
  }

  const pivot = arr[arr.length - 1];
  const left = arr.slice(0, -1).filter(x => x <= pivot);
  const right = arr.slice(0, -1).filter(x => x > pivot);

  return [...quicksortFunctional(left), pivot, ...quicksortFunctional(right)];
}

// ============================================================================
// 4. Three-Way Quicksort (Efficient for duplicates)
// ============================================================================

function quicksort3Way(arr, low = 0, high = arr.length - 1) {
  /**
   * Three-way partitioning: handles duplicates efficiently.
   * Partitions into: < pivot | = pivot | > pivot
   */
  if (low < high) {
    const [lt, gt] = partition3Way(arr, low, high);
    quicksort3Way(arr, low, lt - 1);
    quicksort3Way(arr, gt + 1, high);
  }
  return arr;
}

function partition3Way(arr, low, high) {
  /**
   * Three-way partition (Dutch National Flag algorithm).
   */
  const pivot = arr[low];
  let lt = low;      // arr[low:lt] < pivot
  let i = low + 1;   // current element
  let gt = high;     // arr[gt+1:] > pivot

  while (i <= gt) {
    if (arr[i] < pivot) {
      [arr[lt], arr[i]] = [arr[i], arr[lt]];
      lt++;
      i++;
    } else if (arr[i] > pivot) {
      [arr[i], arr[gt]] = [arr[gt], arr[i]];
      gt--;
    } else {
      i++;
    }
  }

  return [lt, gt];
}

// ============================================================================
// 5. Randomized Quicksort
// ============================================================================

function quicksortRandom(arr, low = 0, high = arr.length - 1) {
  /**
   * Randomized quicksort: picks random pivot to avoid worst case.
   */
  if (low < high) {
    const pivotIdx = partitionRandom(arr, low, high);
    quicksortRandom(arr, low, pivotIdx - 1);
    quicksortRandom(arr, pivotIdx + 1, high);
  }
  return arr;
}

function partitionRandom(arr, low, high) {
  // Randomly pick pivot and swap with last element
  const randomIdx = Math.floor(Math.random() * (high - low + 1)) + low;
  [arr[randomIdx], arr[high]] = [arr[high], arr[randomIdx]];
  
  return partitionLomuto(arr, low, high);
}

// ============================================================================
// Demo and Testing
// ============================================================================

function testQuicksort() {
  const testCases = [
    [8, 3, 1, 7, 0, 10, 2],
    [5, 2, 8, 1, 9],
    [1],
    [],
    [3, 3, 3, 3],
    [5, 4, 3, 2, 1],  // Reverse sorted
    [1, 2, 3, 4, 5],  // Already sorted
    [1, 5, 2, 5, 3, 5, 4],  // Duplicates
  ];

  const implementations = [
    ["Lomuto", quicksortLomuto],
    ["Hoare", quicksortHoare],
    ["Functional", quicksortFunctional],
    ["3-Way", quicksort3Way],
    ["Random", quicksortRandom],
  ];

  console.log("Testing Quicksort Implementations");
  console.log("=".repeat(50));

  implementations.forEach(([name, func]) => {
    console.log(`\n${name} Partition:`);
    let allPassed = true;

    testCases.forEach(test => {
      const arr = [...test];
      const expected = [...test].sort((a, b) => a - b);
      const result = func(arr);

      const passed = JSON.stringify(result) === JSON.stringify(expected);
      if (passed) {
        console.log(`  ✓ [${test}] → [${result}]`);
      } else {
        console.log(`  ✗ [${test}] → [${result}] (expected [${expected}])`);
        allPassed = false;
      }
    });

    console.log(`  Result: ${allPassed ? 'PASSED' : 'FAILED'}`);
  });
}

// Run tests
if (typeof module !== 'undefined' && require.main === module) {
  testQuicksort();

  console.log("\n" + "=".repeat(50));
  console.log("Visual Example:");
  console.log("=".repeat(50));

  const arr = [8, 3, 1, 7, 0, 10, 2];
  console.log(`Original: [${arr}]`);
  quicksortLomuto(arr);
  console.log(`Sorted:   [${arr}]`);
}

// Export for use in other modules
if (typeof module !== 'undefined') {
  module.exports = {
    quicksortLomuto,
    quicksortHoare,
    quicksortFunctional,
    quicksort3Way,
    quicksortRandom,
  };
}
