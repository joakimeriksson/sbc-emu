import sys, urllib.request
for p in sys.argv[2:]:
    data = open(p, 'rb').read()
    req = urllib.request.Request(sys.argv[1] + '/' + p.split('/')[-1], data=data, method='PUT')
    urllib.request.urlopen(req).read()
