"""
Quicksort Implementations in Python
Demonstrates different variations of the quicksort algorithm
"""

# ============================================================================
# 1. Classic Quicksort (In-place, Lomuto partition scheme)
# ============================================================================

def quicksort_lomuto(arr, low=0, high=None):
    """
    In-place quicksort using Lomuto partition scheme.
    Pivot is the last element in the partition.
    
    Time: O(n log n) average, O(n²) worst
    Space: O(log n) for recursion stack
    """
    if high is None:
        high = len(arr) - 1
    
    if low < high:
        # Partition and get pivot position
        pivot_idx = partition_lomuto(arr, low, high)
        
        # Recursively sort left and right partitions
        quicksort_lomuto(arr, low, pivot_idx - 1)
        quicksort_lomuto(arr, pivot_idx + 1, high)
    
    return arr


def partition_lomuto(arr, low, high):
    """
    Lomuto partition: pivot is last element.
    Returns the final position of the pivot.
    """
    pivot = arr[high]  # Choose last element as pivot
    i = low - 1  # Index of smaller element
    
    for j in range(low, high):
        if arr[j] <= pivot:
            i += 1
            arr[i], arr[j] = arr[j], arr[i]  # Swap
    
    # Place pivot in its correct position
    arr[i + 1], arr[high] = arr[high], arr[i + 1]
    return i + 1


# ============================================================================
# 2. Hoare Partition Scheme (Original, more efficient)
# ============================================================================

def quicksort_hoare(arr, low=0, high=None):
    """
    In-place quicksort using Hoare partition scheme (original version).
    Generally more efficient than Lomuto.
    
    Time: O(n log n) average, O(n²) worst
    Space: O(log n) for recursion stack
    """
    if high is None:
        high = len(arr) - 1
    
    if low < high:
        pivot_idx = partition_hoare(arr, low, high)
        quicksort_hoare(arr, low, pivot_idx)
        quicksort_hoare(arr, pivot_idx + 1, high)
    
    return arr


def partition_hoare(arr, low, high):
    """
    Hoare partition: pivot is middle element.
    More efficient than Lomuto (fewer swaps).
    """
    pivot = arr[low]  # Choose first element as pivot
    i = low - 1
    j = high + 1
    
    while True:
        # Move i right until finding element >= pivot
        i += 1
        while arr[i] < pivot:
            i += 1
        
        # Move j left until finding element <= pivot
        j -= 1
        while arr[j] > pivot:
            j -= 1
        
        # If pointers crossed, partitioning is done
        if i >= j:
            return j
        
        # Swap elements at i and j
        arr[i], arr[j] = arr[j], arr[i]


# ============================================================================
# 3. Randomized Quicksort (Avoids worst case on sorted data)
# ============================================================================

import random

def quicksort_random(arr, low=0, high=None):
    """
    Randomized quicksort: picks random pivot to avoid worst case.
    
    Time: O(n log n) expected
    Space: O(log n) for recursion stack
    """
    if high is None:
        high = len(arr) - 1
    
    if low < high:
        pivot_idx = partition_random(arr, low, high)
        quicksort_random(arr, low, pivot_idx - 1)
        quicksort_random(arr, pivot_idx + 1, high)
    
    return arr


def partition_random(arr, low, high):
    """
    Random partition: randomly select pivot, then use Lomuto scheme.
    """
    # Randomly pick pivot and swap with last element
    random_idx = random.randint(low, high)
    arr[random_idx], arr[high] = arr[high], arr[random_idx]
    
    return partition_lomuto(arr, low, high)


# ============================================================================
# 4. Three-Way Quicksort (Efficient for duplicate values)
# ============================================================================

def quicksort_3way(arr, low=0, high=None):
    """
    Three-way partitioning: handles duplicates efficiently.
    Partitions into: < pivot | = pivot | > pivot
    
    Excellent for arrays with many duplicate values.
    Time: O(n log n), O(n) when all elements equal
    """
    if high is None:
        high = len(arr) - 1
    
    if low < high:
        lt, gt = partition_3way(arr, low, high)
        quicksort_3way(arr, low, lt - 1)
        quicksort_3way(arr, gt + 1, high)
    
    return arr


def partition_3way(arr, low, high):
    """
    Three-way partition (Dutch National Flag algorithm).
    Returns (lt, gt) where:
    - arr[low:lt] < pivot
    - arr[lt:gt+1] == pivot
    - arr[gt+1:high+1] > pivot
    """
    pivot = arr[low]
    lt = low       # arr[low:lt] < pivot
    i = low + 1    # current element
    gt = high      # arr[gt+1:] > pivot
    
    while i <= gt:
        if arr[i] < pivot:
            arr[lt], arr[i] = arr[i], arr[lt]
            lt += 1
            i += 1
        elif arr[i] > pivot:
            arr[i], arr[gt] = arr[gt], arr[i]
            gt -= 1
        else:  # arr[i] == pivot
            i += 1
    
    return lt, gt


# ============================================================================
# 5. Iterative Quicksort (Avoids recursion stack overflow)
# ============================================================================

def quicksort_iterative(arr):
    """
    Iterative quicksort using explicit stack.
    Avoids recursion depth limits for very large arrays.
    
    Time: O(n log n) average
    Space: O(log n) for explicit stack
    """
    if len(arr) <= 1:
        return arr
    
    stack = [(0, len(arr) - 1)]
    
    while stack:
        low, high = stack.pop()
        
        if low < high:
            pivot_idx = partition_lomuto(arr, low, high)
            
            # Push right partition first (so left is processed first)
            stack.append((pivot_idx + 1, high))
            stack.append((low, pivot_idx - 1))
    
    return arr


# ============================================================================
# Demo and Testing
# ============================================================================

def test_quicksort():
    """Test all quicksort implementations"""
    test_cases = [
        [8, 3, 1, 7, 0, 10, 2],
        [5, 2, 8, 1, 9],
        [1],
        [],
        [3, 3, 3, 3],
        [5, 4, 3, 2, 1],  # Reverse sorted
        [1, 2, 3, 4, 5],  # Already sorted
        [1, 5, 2, 5, 3, 5, 4],  # Duplicates
    ]
    
    implementations = [
        ("Lomuto", quicksort_lomuto),
        ("Hoare", quicksort_hoare),
        ("Random", quicksort_random),
        ("3-Way", quicksort_3way),
        ("Iterative", quicksort_iterative),
    ]
    
    print("Testing Quicksort Implementations\n" + "="*50)
    
    for name, func in implementations:
        print(f"\n{name} Partition:")
        all_passed = True
        
        for test in test_cases:
            arr = test.copy()
            expected = sorted(test)
            result = func(arr)
            
            if result == expected:
                print(f"  ✓ {test} → {result}")
            else:
                print(f"  ✗ {test} → {result} (expected {expected})")
                all_passed = False
        
        print(f"  Result: {'PASSED' if all_passed else 'FAILED'}")


if __name__ == "__main__":
    test_quicksort()
    
    print("\n" + "="*50)
    print("Visual Example:")
    print("="*50)
    
    arr = [8, 3, 1, 7, 0, 10, 2]
    print(f"Original: {arr}")
    quicksort_lomuto(arr)
    print(f"Sorted:   {arr}")
