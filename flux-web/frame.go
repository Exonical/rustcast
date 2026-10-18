package main

import (
	"fmt"
	"time"
)

type frameMsg struct {
	tsMicros   uint64
	data       []byte
	receivedAt time.Time
	buffers    *frameBufferPool
}

func (f *frameMsg) release() {
	if f.buffers != nil {
		f.buffers.put(f.data)
	}
	f.data = nil
	f.buffers = nil
}

type cursorMsg struct {
	tsMicros uint64
	data     []byte
}

// audioMsg carries one Opus packet: data = [seq u32 BE][opus bytes].
type audioMsg struct {
	tsMicros uint64
	data     []byte
}

func describeNALUs(data []byte) string {
	var types []string
	i := 0
	for i < len(data)-4 {
		if data[i] == 0 && data[i+1] == 0 && data[i+2] == 0 && data[i+3] == 1 {
			if i+4 < len(data) {
				types = append(types, naluTypeName(data[i+4]&0x1F))
			}
			i += 4
		} else if data[i] == 0 && data[i+1] == 0 && data[i+2] == 1 {
			if i+3 < len(data) {
				types = append(types, naluTypeName(data[i+3]&0x1F))
			}
			i += 3
		} else {
			i++
		}
	}
	if len(types) == 0 {
		return fmt.Sprintf("no-start-codes (first 8 bytes: %X)", data[:min(8, len(data))])
	}
	result := ""
	for i, typ := range types {
		if i > 0 {
			result += ", "
		}
		result += typ
	}
	return result
}

func isIDRFrame(data []byte) bool {
	i := 0
	for i < len(data)-4 {
		if data[i] == 0 && data[i+1] == 0 && data[i+2] == 0 && data[i+3] == 1 {
			if i+4 < len(data) && data[i+4]&0x1F == 5 {
				return true
			}
			i += 4
		} else if data[i] == 0 && data[i+1] == 0 && data[i+2] == 1 {
			if i+3 < len(data) && data[i+3]&0x1F == 5 {
				return true
			}
			i += 3
		} else {
			i++
		}
	}
	return false
}

func naluTypeName(typ byte) string {
	switch typ {
	case 1:
		return "P-slice"
	case 5:
		return "IDR"
	case 6:
		return "SEI"
	case 7:
		return "SPS"
	case 8:
		return "PPS"
	case 9:
		return "AUD"
	default:
		return fmt.Sprintf("type-%d", typ)
	}
}
