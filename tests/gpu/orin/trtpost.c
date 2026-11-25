/* trtpost OUT.json SCALE: YOLOv8n's output (trtexec --exportOutput, 1x84x8400) to detections in the
 * original image, as tests/gpu/l4t/trtpost.py does on the Nano (the Orin's initramfs has no Python).
 * Prints "DET class confidence [x,y,w,h]" like tests/gpu/l4t/yolo-input.py's reference. */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

enum { CLASSES = 80, ANCHORS = 8400, MAX = 1024 };

static const char *name(int c) {
    switch (c) {
    case 0: return "person";
    case 2: return "car";
    case 5: return "bus";
    case 7: return "truck";
    case 11: return "stop sign";
    }
    static char n[16];
    snprintf(n, sizeof n, "%d", c);
    return n;
}

struct det { int cls; float conf, x, y, w, h; };

static int by_conf(const void *a, const void *b) {
    float d = ((const struct det *)b)->conf - ((const struct det *)a)->conf;
    return (d > 0) - (d < 0);
}

int main(int argc, char **argv) {
    if (argc < 3)
        return 2;
    FILE *f = fopen(argv[1], "rb");
    if (!f)
        return 3;
    fseek(f, 0, SEEK_END);
    long size = ftell(f);
    fseek(f, 0, SEEK_SET);
    char *text = malloc(size + 1);
    if (!text || fread(text, 1, size, f) != (size_t)size)
        return 4;
    text[size] = 0;
    fclose(f);
    float scale = strtof(argv[2], 0);
    char *p = strstr(text, "\"values\"");
    if (!p)
        return 5;
    p = strchr(p, '[') + 1;
    static float v[(4 + CLASSES) * ANCHORS];
    for (int i = 0; i < (4 + CLASSES) * ANCHORS; i++) {
        char *end;
        v[i] = strtof(p, &end);
        if (end == p)
            return 6;
        p = end + strspn(end, ", \n");
    }
    static struct det d[MAX];
    int n = 0;
    for (int a = 0; a < ANCHORS && n < MAX; a++) {
        int best = 0;
        for (int c = 1; c < CLASSES; c++)
            if (v[(4 + c) * ANCHORS + a] > v[(4 + best) * ANCHORS + a])
                best = c;
        float conf = v[(4 + best) * ANCHORS + a];
        if (conf <= 0.25f)
            continue;
        float x = v[a], y = v[ANCHORS + a], w = v[2 * ANCHORS + a], h = v[3 * ANCHORS + a];
        d[n++] = (struct det){best, conf, (x - w / 2) / scale, (y - h / 2) / scale, w / scale, h / scale};
    }
    qsort(d, n, sizeof *d, by_conf);
    /* non-maximum suppression across classes, IoU 0.45 */
    static char gone[MAX];
    for (int i = 0; i < n; i++) {
        if (gone[i])
            continue;
        printf("DET %s %.2f [%.0f,%.0f,%.0f,%.0f]\n", name(d[i].cls), d[i].conf, d[i].x, d[i].y, d[i].w, d[i].h);
        for (int j = i + 1; j < n; j++) {
            float iw = (d[i].x + d[i].w < d[j].x + d[j].w ? d[i].x + d[i].w : d[j].x + d[j].w) - (d[i].x > d[j].x ? d[i].x : d[j].x);
            float ih = (d[i].y + d[i].h < d[j].y + d[j].h ? d[i].y + d[i].h : d[j].y + d[j].h) - (d[i].y > d[j].y ? d[i].y : d[j].y);
            float inter = (iw > 0 ? iw : 0) * (ih > 0 ? ih : 0);
            if (inter / (d[i].w * d[i].h + d[j].w * d[j].h - inter + 1e-9f) > 0.45f)
                gone[j] = 1;
        }
    }
    return 0;
}
