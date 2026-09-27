"""
Visual Quicksort - Step-by-step diagram
Shows each partition and recursive call
"""

def quicksort_visual(arr, low=0, high=None, depth=0, label=""):
    """
    Quicksort with visual output showing each step
    """
    if high is None:
        high = len(arr) - 1
    
    indent = "  " * depth
    
    # Show current state
    if depth == 0:
        print("=" * 60)
        print("QUICKSORT VISUALIZATION")
        print("=" * 60)
        print(f"Initial array: {arr}\n")
    
    if low >= high:
        return
    
    # Show what we're sorting
    subarray = arr[low:high+1]
    print(f"{indent}{'├─' if depth > 0 else ''}Sorting {label}: {subarray}")
    print(f"{indent}  Range: indices [{low}..{high}]")
    
    # Partition
    pivot_idx = partition_visual(arr, low, high, depth)
    
    # Show result after partition
    print(f"{indent}  After partition: {arr[low:high+1]}")
    print(f"{indent}  Pivot {arr[pivot_idx]} is now at index {pivot_idx}")
    
    # Show the three parts
    left = arr[low:pivot_idx] if pivot_idx > low else []
    pivot = arr[pivot_idx]
    right = arr[pivot_idx+1:high+1] if pivot_idx < high else []
    
    print(f"{indent}  └─ {left} | [{pivot}] | {right}")
    print()
    
    # Recursive calls
    if low < pivot_idx - 1:
        quicksort_visual(arr, low, pivot_idx - 1, depth + 1, "LEFT")
    
    if pivot_idx + 1 < high:
        quicksort_visual(arr, pivot_idx + 1, high, depth + 1, "RIGHT")


def partition_visual(arr, low, high, depth=0):
    """
    Lomuto partition with visual output
    """
    indent = "  " * depth
    pivot = arr[high]
    
    print(f"{indent}  Partitioning around pivot = {pivot} (last element)")
    
    i = low - 1
    
    for j in range(low, high):
        if arr[j] <= pivot:
            i += 1
            if i != j:
                arr[i], arr[j] = arr[j], arr[i]
                print(f"{indent}    Swap: {arr[j]} ↔ {arr[i]} → {arr[low:high+1]}")
    
    # Place pivot
    arr[i + 1], arr[high] = arr[high], arr[i + 1]
    if i + 1 != high:
        print(f"{indent}    Place pivot: {arr[high]} → index {i+1}")
    
    return i + 1


def draw_quicksort_tree(arr):
    """
    ASCII tree visualization of the quicksort recursion tree
    """
    print("\n" + "=" * 60)
    print("RECURSION TREE VISUALIZATION")
    print("=" * 60)
    print()
    
    def build_tree(arr_segment, depth=0):
        if len(arr_segment) <= 1:
            return
        
        indent = "  " * depth
        connector = "├─ " if depth > 0 else ""
        
        # Choose pivot (last element)
        pivot = arr_segment[-1]
        left = [x for x in arr_segment[:-1] if x <= pivot]
        right = [x for x in arr_segment[:-1] if x > pivot]
        
        print(f"{indent}{connector}{arr_segment}")
        print(f"{indent}   pivot: {pivot}")
        
        if left:
            print(f"{indent}   ├─ left:  {left}")
            build_tree(left, depth + 1)
        
        if right:
            print(f"{indent}   └─ right: {right}")
            build_tree(right, depth + 1)
    
    build_tree(arr)


def draw_partition_process(arr):
    """
    Detailed view of a single partition operation
    """
    print("\n" + "=" * 60)
    print("DETAILED PARTITION PROCESS")
    print("=" * 60)
    print(f"Array: {arr}")
    print(f"Pivot: {arr[-1]} (last element)")
    print()
    
    pivot = arr[-1]
    i = -1
    
    print("Step-by-step partition:")
    print(f"  Initial: {arr}")
    print(f"  i = {i} (boundary of smaller elements)")
    print()
    
    for j, val in enumerate(arr[:-1]):
        print(f"  j={j}, arr[j]={val}")
        
        if val <= pivot:
            i += 1
            if i != j:
                arr[i], arr[j] = arr[j], arr[i]
                print(f"    {val} ≤ {pivot} → swap arr[{i}]↔arr[{j}]")
                print(f"    Result: {arr}")
            else:
                print(f"    {val} ≤ {pivot} → already in place")
        else:
            print(f"    {val} > {pivot} → skip")
        print()
    
    # Place pivot
    arr[i + 1], arr[-1] = arr[-1], arr[i + 1]
    print(f"  Place pivot at index {i+1}: {arr}")
    print()
    print(f"  Final partition:")
    print(f"    Smaller: {arr[:i+1]}")
    print(f"    Pivot:   [{arr[i+1]}]")
    print(f"    Larger:  {arr[i+2:]}")


def ascii_array_visual(arr, pivot_idx=None, left_bound=0, right_bound=None):
    """
    Draw array with visual indicators
    """
    if right_bound is None:
        right_bound = len(arr) - 1
    
    # Top border
    print("  ┌" + "─┬".join(["───"] * len(arr)) + "─┐")
    
    # Values
    values = "  │"
    for i, val in enumerate(arr):
        values += f" {val:^2}│"
    print(values)
    
    # Bottom border
    print("  └" + "─┴".join(["───"] * len(arr)) + "─┘")
    
    # Indices
    indices = "   "
    for i in range(len(arr)):
        indices += f" {i:^2} "
    print(indices)
    
    # Markers
    if pivot_idx is not None:
        marker = "   "
        for i in range(len(arr)):
            if i == pivot_idx:
                marker += " ↑P "
            elif i == left_bound:
                marker += " ↑L "
            elif i == right_bound:
                marker += " ↑R "
            else:
                marker += "    "
        print(marker)


def main():
    # Example 1: Small array with full visualization
    arr1 = [8, 3, 1, 7, 0, 10, 2]
    print("\n" + "█" * 60)
    print("EXAMPLE 1: Complete Visualization")
    print("█" * 60)
    quicksort_visual(arr1.copy())
    
    print("\n" + "█" * 60)
    print(f"Final sorted array: {arr1}")
    print("█" * 60)
    
    # Example 2: Partition detail
    arr2 = [8, 3, 1, 7, 0, 10, 2]
    draw_partition_process(arr2.copy())
    
    # Example 3: Recursion tree
    arr3 = [8, 3, 1, 7, 0, 10, 2]
    draw_quicksort_tree(arr3.copy())
    
    # Example 4: ASCII visual
    print("\n" + "=" * 60)
    print("ASCII ARRAY VISUALIZATION")
    print("=" * 60)
    arr4 = [8, 3, 1, 7, 0, 10, 2]
    print("\nInitial array:")
    ascii_array_visual(arr4, pivot_idx=6, left_bound=0, right_bound=6)
    
    print("\nAfter first partition:")
    arr4_partitioned = [1, 0, 2, 7, 3, 10, 8]
    ascii_array_visual(arr4_partitioned, pivot_idx=2)
    
    # Example 5: Different arrays
    print("\n\n" + "=" * 60)
    print("MORE EXAMPLES")
    print("=" * 60)
    
    test_arrays = [
        [5, 2, 8, 1, 9],
        [3, 3, 3, 3],
        [5, 4, 3, 2, 1],
    ]
    
    for arr in test_arrays:
        print(f"\n{'─' * 40}")
        print(f"Sorting: {arr}")
        print('─' * 40)
        quicksort_visual(arr.copy())


main()
if __name__ == "__main__":
    main()
