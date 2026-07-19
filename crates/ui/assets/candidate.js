function updateCandidates(candidates) {
    const candidateList = document.getElementById('candidate-list');

    const existingItems = Array.from(candidateList.children);

    candidates.forEach((candidate, index) => {
        if (existingItems[index]) {
            existingItems[index].textContent = candidate;
        } else {
            const li = document.createElement('li');
            li.textContent = candidate;
            candidateList.appendChild(li);
        }
    });

    while (existingItems.length > candidates.length) {
        candidateList.removeChild(existingItems.pop());
    }
}

function updateSelection(index) {
    const candidateList = document.getElementById('candidate-list');
    const selected = candidateList.querySelector('[data-selected]');
    if (selected) {
        selected.removeAttribute('data-selected');
    }

    // guard against an index past the (possibly just-shrunk) list and an empty
    // list: children[index] / children[0] would be undefined and throw, which
    // aborts the script and leaves the highlight and scroll stuck
    if (index < 0 || index >= candidateList.children.length) {
        return;
    }

    candidateList.children[index].setAttribute('data-selected', '');

    const itemHeight = candidateList.children[0].offsetHeight;
    const visibleItems = Math.floor(candidateList.clientHeight / itemHeight);

    const groupSize = 5;
    const groupIndex = Math.floor(index / groupSize);
    const scrollToIndex = groupIndex * groupSize;

    if (index === scrollToIndex || !isElementInView(candidateList.children[index], candidateList)) {
        candidateList.children[scrollToIndex].scrollIntoView({ behavior: "instant", block: "start", inline: "start" });
    }
}

function isElementInView(element, container) {
    const containerRect = container.getBoundingClientRect();
    const elementRect = element.getBoundingClientRect();

    return (
        elementRect.top >= containerRect.top &&
        elementRect.bottom <= containerRect.bottom
    );
}

function adjustWindowSize() {
    const candidateList = document.getElementById('candidate-list');

    // Clear any existing items
    candidateList.innerHTML = '';

    // Add 5 test items to measure
    for (let i = 0; i < 5; i++) {
        const li = document.createElement('li');
        li.textContent = `Item ${i + 1}`;
        candidateList.appendChild(li);
    }

    // Calculate heights
    const footer = document.querySelector('footer');
    const main = document.querySelector('main');
    const body = document.body;

    // Get the height of a single item
    const itemHeight = candidateList.children[0].offsetHeight;

    // Calculate the height needed for exactly 5 items
    const candidateListHeight = itemHeight * 5;
    const footerHeight = footer.offsetHeight;
    const mainPadding = parseInt(window.getComputedStyle(main).paddingTop) +
        parseInt(window.getComputedStyle(main).paddingBottom);
    const bodyPadding = parseInt(window.getComputedStyle(body).paddingTop) +
        parseInt(window.getComputedStyle(body).paddingBottom);

    // Calculate total window height needed
    const totalHeight = candidateListHeight + footerHeight + mainPadding + bodyPadding;

    // Clear the test items
    candidateList.innerHTML = '';

    window.ipc.postMessage(JSON.stringify({
        type: 'resize',
        height: totalHeight
    }));
}

window.addEventListener('DOMContentLoaded', () => {
    setTimeout(adjustWindowSize, 50); // Small delay to ensure rendering is complete
});
